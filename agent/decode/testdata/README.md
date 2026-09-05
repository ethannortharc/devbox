# TLS ClientHello fixtures

Two byte-for-byte TLS records, each 1521 bytes, used by `decode_test.go` and by
`agent/capture/packet_test.go`. Both are *whole* ClientHellos; the tests are the
thing that cuts them, at byte 1448, to reproduce the split the parser has to
survive.

## Why 1448

`agent/capture` observed this on the `devtest` box, one `curl https://example.com`,
one flow:

```
06:51:59.593422 IP 192.168.5.15.37928 > 172.66.147.243.443: Flags [.],  seq 1:1449,    ack 1, ..., length 1448
06:51:59.593435 IP 192.168.5.15.37928 > 172.66.147.243.443: Flags [P.], seq 1449:1561, ack 1, ..., length 112
```

A 1560-byte ClientHello against a 1448-byte MSS. It is that large because TLS
1.3 now offers a post-quantum key share by default: `X25519MLKEM768` alone is
1216 bytes of the 1521 here. The fixtures reproduce the shape, not the exact
byte count — 1521 against the same 1448 MSS splits the same way.

## `clienthello_pq.bin` — server_name in the first segment

Not hand-built. Captured from Go's own `crypto/tls` client (go1.26.2), which
offers `X25519MLKEM768` by default, so the extension shape is a real stack's and
not one invented to make the test pass:

```go
client, server := net.Pipe()
done := make(chan []byte, 1)
go func() {
	buf := make([]byte, 64*1024)
	n, _ := io.ReadAtLeast(server, buf, 5)
	for want := 5 + int(buf[3])<<8 + int(buf[4]); n < want; {
		m, err := server.Read(buf[n:])
		n += m
		if err != nil {
			break
		}
	}
	done <- buf[:n]
	server.Close()
}()
c := tls.Client(client, &tls.Config{
	ServerName: "example.com",
	NextProtos: []string{"h2", "http/1.1"},
})
_ = c.Handshake() // fails: nothing answers. The ClientHello is already written.
client.Close()
os.Stdout.Write(<-done)
```

Extensions, in wire order, with their byte offsets in the record:

| ext | | bytes |
|---|---|---|
| `0x0000` | server_name | 108‥128 |
| `0x000b` | ec_point_formats | 128‥134 |
| `0xff01` | renegotiation_info | 134‥139 |
| `0x0017` | extended_master_secret | 139‥143 |
| `0x0012` | signed_certificate_timestamp | 143‥147 |
| `0x0005` | status_request | 147‥156 |
| `0x000a` | supported_groups | 156‥176 |
| `0x000d` | signature_algorithms | 176‥202 |
| `0x0032` | signature_algorithms_cert | 202‥232 |
| `0x0010` | ALPN | 232‥250 |
| `0x002b` | supported_versions | 250‥259 |
| `0x0033` | key_share (`0x11ec` X25519MLKEM768, 1216 B; `0x001d` X25519, 32 B) | 259‥1521 |

server_name at 108 means the first 1448-byte segment already carries it. This is
the common case, and the one that makes reassembly the exception rather than the
rule: the parser answers from segment one and holds nothing.

## `clienthello_pq_sni_last.bin` — server_name in the second segment

`clienthello_pq.bin` with the `server_name` entry moved to the end of the
extension block and nothing else touched: same 1521 bytes, same lengths at every
level, extensions still a valid permutation (TLS 1.3 fixes the order of no
extension except `pre_shared_key`, which is absent here). server_name lands at
bytes 1501‥1521, so a 1448-byte cut puts it wholly in the second segment and
only the join can read it.

```python
entries = split_extension_block(ext)
reordered = [e for e in entries if e.type != 0x0000] + [e for e in entries if e.type == 0x0000]
```

## The third case is not a file

A record claiming 65535 bytes and supplying 100 is built inline from
`clienthello_pq.bin` in the tests that need it. Storing 100 bytes as a file
would hide the one thing worth reading, which is the lie in the length field.

---

© 2026 Ethan H.B. Zhou
