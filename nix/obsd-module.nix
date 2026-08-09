# NixOS module for devbox-obsd, the in-guest observability agent (§7.3).
#
# This module supervises the agent. It is version-pinned to the host binary —
# the collector refuses a mismatched agent rather than decoding events with the
# wrong layout.
#
# **Not yet wired into provisioning.** Nothing pushes the `devbox-obsd` binary
# into a box, imports this module, or sets `services.devbox-obsd.enable`, so no
# box built by `devbox create` runs the agent today. Two things depend on that
# and do not work until it lands:
#
#   * activity capture — the console's timeline stays empty on a real box;
#   * domain allowlists — `allowlist` and `mirror-only` are enforced by
#     nftables whose allow set only the agent can populate from DNS, so an
#     allowlisted domain is *blocked*, not permitted.
#
# `devbox policy set` refuses domain-based postures for exactly that reason
# (see `policy::enforce`). CIDR-only allowlists, `isolated`, and `open` are
# fully enforced without the agent.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.devbox-obsd;
in
{
  options.services.devbox-obsd = {
    enable = lib.mkEnableOption "the devbox observability agent";

    package = lib.mkOption {
      type = lib.types.path;
      default = "/usr/local/bin/devbox-obsd";
      description = ''
        Path to the agent binary. Defaults to where `devbox` pushes it on
        provision, rather than a nixpkgs derivation, because the agent must
        match the *host* binary's build, not the guest's channel.
      '';
    };

    boxId = lib.mkOption {
      type = lib.types.str;
      description = "Box identifier stamped onto every event.";
    };

    socket = lib.mkOption {
      type = lib.types.str;
      default = "/run/devbox/obsd.sock";
      description = ''
        Unix socket the collector listens on. `devbox` bind-mounts the host
        side here; on VM runtimes this becomes a vsock address instead.
      '';
    };

    enableEbpf = lib.mkOption {
      type = lib.types.bool;
      # The shipped agent has no eBPF source linked in — `chooseSource` errors
      # out when neither a fixture nor `-no-ebpf` is given, so defaulting this
      # on would restart-loop the service. Flip it when the privileged build
      # lands; until then the degraded path is the one that runs.
      default = false;
      description = ''
        Attach eBPF programs. Turning this off selects the degraded
        proc-polling path (§13), which sees processes and sockets but not
        DNS, TLS, or file access — the console marks which source is in play.
      '';
    };

    capture = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "exec" "connect" "dns" "tls" "file" ];
      description = ''
        Event domains to capture. `api` is opt-in and off by default: it means
        an SSL uprobe or a MITM proxy, which is a different trust decision from
        watching syscalls (§7.1, N3).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # CO-RE needs BTF in the running kernel. Without it the agent can still
    # run, but only on the degraded path — so make the requirement explicit
    # rather than letting it fail at attach time.
    boot.kernelPatches = lib.mkIf cfg.enableEbpf [ ];
    boot.kernel.sysctl."kernel.unprivileged_bpf_disabled" = lib.mkDefault 1;

    environment.systemPackages = with pkgs; [
      # Present so `devbox doctor` can check BTF, nftables, and map state from
      # inside the box (§17).
      bpftools
      nftables
    ];

    systemd.services.devbox-obsd = {
      description = "devbox observability agent";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.concatStringsSep " " (
          [
            cfg.package
            "-box-id" (lib.escapeShellArg cfg.boxId)
            "-socket" (lib.escapeShellArg cfg.socket)
          ]
          ++ lib.optional (!cfg.enableEbpf) "-no-ebpf"
          # The egress policy, when the control plane has written one. The
          # agent is what keeps the firewall's allow sets in step with DNS, so
          # an allowlist is only enforceable if this is passed.
          # No `pathExists` here. It is evaluated while *building* the
          # configuration, so a module enabled before the first policy is
          # staged bakes in a unit that never passes `-policy` — and writing
          # the file later does not reevaluate Nix. The agent tolerates an
          # absent file (it enforces nothing until one appears), so the flag
          # is unconditional and the decision happens at run time where it
          # belongs.
          ++ [ "-policy" "/etc/devbox/policy.json" ]
        );

        # Loading eBPF programs and reading every process needs real
        # privileges; everything else is dropped.
        AmbientCapabilities = lib.mkIf cfg.enableEbpf [
          "CAP_BPF"
          "CAP_PERFMON"
          "CAP_SYS_RESOURCE"
          "CAP_NET_ADMIN"
          "CAP_SYSLOG"
        ];
        # The bounding set is *unconditional*; only the eBPF entries are not.
        #
        # `lib.mkIf cfg.enableEbpf` removed the whole option when eBPF was
        # off — which is the default — and a unit with no `User` and no
        # bounding set runs as root with the full capability set. So the
        # configuration that looks like the restricted one was the least
        # restricted of the two, and `NoNewPrivileges` does not help: it stops
        # a process *gaining* capabilities, not holding the ones it started
        # with.
        #
        # CAP_SYSLOG is needed either way, because reading /dev/kmsg for the
        # firewall's record of refused connections has nothing to do with eBPF.
        CapabilityBoundingSet = [
          # Reading /dev/kmsg, which is where the firewall's record of a
          # refused connection lives. Under `kernel.dmesg_restrict=1` — the
          # default on most distributions — that read needs CAP_SYSLOG, and
          # without it the netfilter source reports itself unsupported and is
          # skipped. Silently: the agent keeps running, capture keeps working,
          # and policy events simply never appear, which looks exactly like a
          # box that never violated its policy.
          "CAP_SYSLOG"
        ] ++ lib.optionals cfg.enableEbpf [
          "CAP_BPF"
          "CAP_PERFMON"
          "CAP_SYS_RESOURCE"
          "CAP_NET_ADMIN"
        ];

        # The agent observes; it has no business writing anywhere except its
        # socket directory.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        RuntimeDirectory = "devbox";
        RuntimeDirectoryMode = "0750";

        # The collector is the source of truth; a crashed agent should come
        # back rather than leaving a silent gap in the timeline.
        Restart = "always";
        RestartSec = "2s";

        # An observability agent that starves the box it observes has failed
        # at its job. The §7.3 budget is <3% CPU at 10k events/s; this is the
        # hard stop if a probe ever misbehaves.
        CPUQuota = "25%";
        MemoryMax = "256M";
      };
    };
  };
}
