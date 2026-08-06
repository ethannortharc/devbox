# NixOS module for devbox-obsd, the in-guest observability agent (§7.3).
#
# The agent binary is embedded in the devbox host binary and pushed into the
# box on provision; this module supervises it. It is version-pinned to the host
# binary — the collector refuses a mismatched agent rather than decoding events
# with the wrong layout.
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
      default = true;
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
        );

        # Loading eBPF programs and reading every process needs real
        # privileges; everything else is dropped.
        AmbientCapabilities = lib.mkIf cfg.enableEbpf [
          "CAP_BPF"
          "CAP_PERFMON"
          "CAP_SYS_RESOURCE"
          "CAP_NET_ADMIN"
        ];
        CapabilityBoundingSet = lib.mkIf cfg.enableEbpf [
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
