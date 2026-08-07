package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strings"
	"testing"

	"github.com/ethannortharc/devbox/ztpd/statemachine"
)

func server(t *testing.T) (*Server, http.Handler) {
	t.Helper()

	catalog := &MapCatalog{
		Serials: map[string]DeviceIdentity{
			"SN-LEAF-001": {Name: "leaf1", Role: "leaf"},
			"SN-LEAF-002": {Name: "leaf2", Role: "leaf"},
		},
		Configs: map[string]string{
			"leaf1": "hostname leaf1\nrouter bgp 65000\n",
			"leaf2": "hostname leaf2\nrouter bgp 65001\n",
		},
	}
	s := New(statemachine.NewRegistry(), catalog, "http://10.0.0.1:8080")
	return s, s.Handler()
}

func do(t *testing.T, h http.Handler, method, path, body string) *httptest.ResponseRecorder {
	t.Helper()

	var reader *strings.Reader
	if body == "" {
		reader = strings.NewReader("")
	} else {
		reader = strings.NewReader(body)
	}
	req := httptest.NewRequest(method, path, reader)
	req.Header.Set("Content-Type", "application/json")

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

// walk provisions a serial all the way to healthy through the HTTP API.
func walk(t *testing.T, h http.Handler, serial string) {
	t.Helper()

	rec := do(t, h, "POST", "/identify", `{"serial":"`+serial+`"}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("identify %s: %d %s", serial, rec.Code, rec.Body)
	}
	for _, state := range []string{"rendering", "pushing", "verifying", "healthy"} {
		rec := do(t, h, "POST", "/status",
			`{"serial":"`+serial+`","state":"`+state+`"}`)
		if rec.Code != http.StatusOK {
			t.Fatalf("status %s→%s: %d %s", serial, state, rec.Code, rec.Body)
		}
	}
}

func TestTheBootstrapScriptIsSelfContained(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	rec := do(t, h, "GET", "/bootstrap.sh", "")

	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d", rec.Code)
	}
	body := rec.Body.String()

	// A blank node has no interpreter to install and no package manager yet.
	if !strings.HasPrefix(body, "#!/bin/sh") {
		t.Error("the bootstrap must be plain sh; a blank node has nothing else")
	}
	// It has to know where to come back to, and it has to report both success
	// and failure — a script that only reports success leaves a node stuck in
	// `discovered` forever.
	for _, needed := range []string{
		"http://10.0.0.1:8080", "/identify", "/config/", "report healthy", "report failed",
	} {
		if !strings.Contains(body, needed) {
			t.Errorf("the bootstrap script is missing %q", needed)
		}
	}
}

func TestIdentifyResolvesASerialAndAdvancesTheNode(t *testing.T) {
	t.Parallel()

	s, h := server(t)
	rec := do(t, h, "POST", "/identify", `{"serial":"SN-LEAF-001"}`)

	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d: %s", rec.Code, rec.Body)
	}
	var resp IdentifyResponse
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if resp.Name != "leaf1" || resp.Role != "leaf" {
		t.Errorf("resp = %+v", resp)
	}
	if !strings.HasSuffix(resp.ConfigURL, "/config/leaf1") {
		t.Errorf("config URL = %q", resp.ConfigURL)
	}

	node, ok := s.registry.Get("SN-LEAF-001")
	if !ok || node.State != statemachine.Identified {
		t.Errorf("node = %+v", node)
	}
}

func TestAnUnknownSerialIsRecordedAsFailedNotSilentlyIgnored(t *testing.T) {
	t.Parallel()

	// A device on the network that the source of truth does not know about is
	// exactly what an operator wants to see.
	s, h := server(t)
	rec := do(t, h, "POST", "/identify", `{"serial":"SN-STRANGER"}`)

	if rec.Code != http.StatusNotFound {
		t.Errorf("code = %d, want 404", rec.Code)
	}
	node, ok := s.registry.Get("SN-STRANGER")
	if !ok {
		t.Fatal("the stranger should still be recorded")
	}
	if node.State != statemachine.Failed {
		t.Errorf("state = %s, want failed", node.State)
	}
	if !strings.Contains(node.Reason, "source of truth") {
		t.Errorf("reason = %q", node.Reason)
	}
}

func TestIdentifyRejectsAMalformedRequest(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	for _, body := range []string{"{not json}", `{"serial":""}`} {
		rec := do(t, h, "POST", "/identify", body)
		if rec.Code != http.StatusBadRequest {
			t.Errorf("body %q: code = %d, want 400", body, rec.Code)
		}
	}
}

func TestConfigIsServedWithAHashSoRepushCanBeSkipped(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	rec := do(t, h, "GET", "/config/leaf1", "")

	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "hostname leaf1") {
		t.Errorf("body = %q", rec.Body)
	}
	hash := rec.Header().Get("X-Devbox-Config-Hash")
	if hash == "" {
		t.Error("without a hash, a node cannot tell 'same config' from 'changed'")
	}

	// The same config always hashes the same; a different one does not.
	if hash != Hash("hostname leaf1\nrouter bgp 65000\n") {
		t.Errorf("hash = %q", hash)
	}
	if hash == Hash("hostname leaf2\nrouter bgp 65001\n") {
		t.Error("two different configs must not share a hash")
	}
}

func TestAnUnknownDeviceHasNoConfig(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	if rec := do(t, h, "GET", "/config/ghost", ""); rec.Code != http.StatusNotFound {
		t.Errorf("code = %d, want 404", rec.Code)
	}
}

func TestStatusReportsDriveTheStateMachine(t *testing.T) {
	t.Parallel()

	s, h := server(t)
	walk(t, h, "SN-LEAF-001")

	node, _ := s.registry.Get("SN-LEAF-001")
	if node.State != statemachine.Healthy {
		t.Errorf("state = %s, want healthy", node.State)
	}
}

func TestAnIllegalTransitionIsAConflictNotASilentAccept(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	walk(t, h, "SN-LEAF-001")

	// A healthy node reporting `verifying` means the node and the server
	// disagree about where provisioning is.
	rec := do(t, h, "POST", "/status", `{"serial":"SN-LEAF-001","state":"verifying"}`)
	if rec.Code != http.StatusConflict {
		t.Errorf("code = %d, want 409", rec.Code)
	}
}

func TestAnUnknownStateIsRejected(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	do(t, h, "POST", "/identify", `{"serial":"SN-LEAF-001"}`)

	rec := do(t, h, "POST", "/status", `{"serial":"SN-LEAF-001","state":"vibing"}`)
	if rec.Code != http.StatusBadRequest {
		t.Errorf("code = %d, want 400", rec.Code)
	}
}

func TestStatusEndpointReportsConvergence(t *testing.T) {
	t.Parallel()

	_, h := server(t)

	// Nothing provisioned yet: not converged.
	rec := do(t, h, "GET", "/status", "")
	var before map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &before); err != nil {
		t.Fatal(err)
	}
	if before["converged"] != false {
		t.Errorf("an empty fabric must not report converged: %v", before)
	}

	walk(t, h, "SN-LEAF-001")
	walk(t, h, "SN-LEAF-002")

	rec = do(t, h, "GET", "/status", "")
	var after map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &after); err != nil {
		t.Fatal(err)
	}
	if after["converged"] != true {
		t.Errorf("every node is healthy, so the fabric has converged: %v", after)
	}
	if after["healthy"].(float64) != 2 {
		t.Errorf("healthy = %v", after["healthy"])
	}
}

func TestMetricsExposeTheSLOs(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	walk(t, h, "SN-LEAF-001")

	rec := do(t, h, "GET", "/metrics", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d", rec.Code)
	}
	body := rec.Body.String()

	// §10.3's SLOs: provisioning time and fabric convergence.
	for _, needed := range []string{
		"# TYPE ztp_fabric_converged gauge",
		"node_provision_seconds{quantile=\"0.95\"}",
		"ztp_nodes_total 1",
		"ztp_node_attempts{node=\"leaf1\"} 1",
	} {
		if !strings.Contains(body, needed) {
			t.Errorf("metrics are missing %q:\n%s", needed, body)
		}
	}

	// Every state must appear even at zero, or it cannot be alerted on before
	// it first fires.
	for _, state := range append(append([]statemachine.State{}, statemachine.Order...),
		statemachine.Failed) {
		if !strings.Contains(body, "ztp_nodes_by_state{state=\""+string(state)+"\"}") {
			t.Errorf("state %s is missing from the metrics", state)
		}
	}
}

func TestHealthzAnswersWithoutAnyState(t *testing.T) {
	t.Parallel()

	_, h := server(t)
	rec := do(t, h, "GET", "/healthz", "")
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), "ok") {
		t.Errorf("healthz = %d %q", rec.Code, rec.Body)
	}
}

// The §10.3 chaos requirement, at the HTTP layer.
func TestANodeThatFailsHalfwayCanStartOverAndReachHealthy(t *testing.T) {
	t.Parallel()

	s, h := server(t)

	// First attempt gets as far as pushing, then dies.
	do(t, h, "POST", "/identify", `{"serial":"SN-LEAF-001"}`)
	do(t, h, "POST", "/status", `{"serial":"SN-LEAF-001","state":"rendering"}`)
	do(t, h, "POST", "/status", `{"serial":"SN-LEAF-001","state":"pushing"}`)
	rec := do(t, h, "POST", "/status",
		`{"serial":"SN-LEAF-001","state":"failed","reason":"ztpd went away"}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("failing should be accepted: %d %s", rec.Code, rec.Body)
	}

	// It comes back and provisions cleanly.
	walk(t, h, "SN-LEAF-001")

	node, _ := s.registry.Get("SN-LEAF-001")
	if node.State != statemachine.Healthy {
		t.Errorf("state = %s; recovery should be idempotent", node.State)
	}
	if node.Attempts != 2 {
		t.Errorf("attempts = %d, want 2", node.Attempts)
	}
	if !s.registry.Summarize().Converged {
		t.Error("the fabric should have converged after recovery")
	}
}

// TestProvisioningListenerHasNoMetrics pins the port separation.
//
// Blank devices boot from the provisioning network, and /metrics carries every
// node name and its state. Serving it there handed anything on that network a
// fabric inventory — which is what the separate -metrics listener exists to
// prevent, so the separation has to be real and not just configured.
func TestProvisioningListenerHasNoMetrics(t *testing.T) {
	t.Parallel()

	s := New(statemachine.NewRegistry(), &MapCatalog{}, "http://ztp.example")

	// Both operator routes, not just metrics: GET /status returns the same
	// inventory by another path, and moving one without the other left the
	// exposure exactly where it was.
	for _, path := range []string{"/metrics", "/status"} {
		req := httptest.NewRequest(http.MethodGet, path, nil)
		rec := httptest.NewRecorder()
		s.ProvisioningHandler().ServeHTTP(rec, req)
		// Not 200 is the property that matters: GET /status returns 405
		// rather than 404 because POST /status is still registered there,
		// and either way no inventory crosses the wire.
		if rec.Code == http.StatusOK {
			t.Errorf("%s served an inventory on the provisioning listener", path)
		}
	}

	// POST /status is a node reporting its own progress, and must still work.
	req := httptest.NewRequest(http.MethodPost, "/status", strings.NewReader("{}"))
	rec := httptest.NewRecorder()
	s.ProvisioningHandler().ServeHTTP(rec, req)
	if rec.Code == http.StatusNotFound {
		t.Error("nodes must still be able to report status")
	}

	// And the node routes it does need still work.
	req = httptest.NewRequest(http.MethodGet, "/healthz", nil)
	rec = httptest.NewRecorder()
	s.ProvisioningHandler().ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Errorf("healthz should serve on the provisioning listener: got %d", rec.Code)
	}

	// The metrics listener still has them.
	req = httptest.NewRequest(http.MethodGet, "/metrics", nil)
	rec = httptest.NewRecorder()
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Errorf("metrics should serve on its own listener: got %d", rec.Code)
	}
}

// TestOperatorListenerServesTheInventory is the other half of
// TestProvisioningListenerHasNoMetrics.
//
// Asserting a route is absent from one listener says nothing about whether it
// is present on the other, and that gap is exactly how GET /status came to be
// reachable on neither while the documentation said otherwise.
func TestOperatorListenerServesTheInventory(t *testing.T) {
	t.Parallel()

	s := New(statemachine.NewRegistry(), &MapCatalog{}, "http://ztp.example")

	for _, path := range []string{"/metrics", "/status"} {
		req := httptest.NewRequest(http.MethodGet, path, nil)
		rec := httptest.NewRecorder()
		s.OperatorHandler().ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Errorf("%s must serve on the operator listener: got %d", path, rec.Code)
		}
	}
}

// TestOperatorListenerTakesNoProvisioningInput is the other half again.
//
// Making the operator handler a *superset* of the provisioning one was an
// earlier attempt at keeping the two sets complementary; a superset is not a
// complement, and it put identity and status submission on the management
// network. Neither listener may carry the other's routes.
func TestOperatorListenerTakesNoProvisioningInput(t *testing.T) {
	t.Parallel()

	s := New(statemachine.NewRegistry(), &MapCatalog{}, "http://ztp.example")

	for _, tc := range []struct{ method, path string }{
		{http.MethodPost, "/identify"},
		{http.MethodPost, "/status"},
		{http.MethodGet, "/config/leaf1"},
		{http.MethodGet, "/bootstrap.sh"},
	} {
		req := httptest.NewRequest(tc.method, tc.path, strings.NewReader("{}"))
		rec := httptest.NewRecorder()
		s.OperatorHandler().ServeHTTP(rec, req)
		if rec.Code == http.StatusOK {
			t.Errorf("%s %s must not serve on the operator listener", tc.method, tc.path)
		}
	}
}

// TestBootstrapScriptIsValidShell parses the script the nodes run.
//
// It lives in a Go raw string, is assembled with Fprintf, and only ever
// executes on a blank device that has just booted — so nothing in this
// repository would notice a syntax error until a fabric failed to provision.
// It has been broken three times during development, every time by a backtick
// in a comment terminating the raw string, and once by a `%s` in added shell
// being eaten as a format verb.
func TestBootstrapScriptIsValidShell(t *testing.T) {
	t.Parallel()

	s := New(statemachine.NewRegistry(), &MapCatalog{}, "http://ztp.example")
	req := httptest.NewRequest(http.MethodGet, "/bootstrap.sh", nil)
	rec := httptest.NewRecorder()
	s.ProvisioningHandler().ServeHTTP(rec, req)

	script := rec.Body.String()
	if !strings.HasPrefix(script, "#!/bin/sh") {
		t.Fatalf("bootstrap script does not start with a shebang: %.40q", script)
	}
	// The advertised URL must have been substituted, not left as a verb.
	if strings.Contains(script, "%s") || strings.Contains(script, "%!") {
		t.Error("bootstrap script contains an unsubstituted or malformed format verb")
	}
	if !strings.Contains(script, "http://ztp.example") {
		t.Error("bootstrap script does not carry the advertised URL")
	}

	cmd := exec.Command("sh", "-n")
	cmd.Stdin = strings.NewReader(script)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Errorf("bootstrap script is not valid shell: %v\n%s", err, out)
	}
}

// TestBootstrapSourceHasNoBackticks guards the raw string that holds it.
//
// I have broken this file five times by writing a backtick inside the
// bootstrap script's comments — Go ends the raw string there, and the error
// surfaces dozens of lines away as "unexpected name". The compiler does catch
// it, but only after a build cycle, and the message never points at the cause.
// This says the cause.
func TestBootstrapSourceHasNoBackticks(t *testing.T) {
	t.Parallel()

	src, err := os.ReadFile("server.go")
	if err != nil {
		t.Skipf("cannot read own source: %v", err)
	}
	text := string(src)

	start := strings.Index(text, "fmt.Fprintf(w, `#!/bin/sh")
	if start < 0 {
		t.Fatal("could not find the bootstrap script; this guard needs updating")
	}
	end := strings.Index(text[start:], "`, s.bootURL)")
	if end < 0 {
		t.Fatal("bootstrap raw string is not terminated where expected")
	}

	// One backtick opens it and one closes it; anything else is inside.
	body := text[start : start+end]
	if n := strings.Count(body, "`"); n != 1 {
		t.Errorf("bootstrap script contains %d backticks; a backtick anywhere "+
			"inside — including in a comment — ends the Go raw string. Use "+
			"double quotes when naming a command.", n)
	}
}
