# NixOS module for devbox-obsd, the in-guest observability agent (§7.3).
#
# This module supervises the agent. It is version-pinned to the host binary —
# the collector refuses a mismatched agent rather than decoding events with the
# wrong layout.
#
# Provisioning writes this module and the version-matched embedded agent before
# `nixos-rebuild`, then mounts the host collector directory at
# `/run/devbox-host` on native Linux Docker; VM-backed runtimes use exec stdio.
# Domain allowlists are admitted only after the agent's atomic status file
# confirms that packet capture actually acquired its kernel resources.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.devbox-obsd;
in
{
  options.services.devbox-obsd = {
    enable = lib.mkEnableOption "the devbox observability agent";

    package = lib.mkOption {
      type = lib.types.str;
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
        side here only when native Linux Docker shares the host kernel.
      '';
    };

    noTransport = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Capture and enforce without exporting events. VM services use this
        between console sessions; the console launches a second agent over
        authenticated runtime-exec stdio because host Unix sockets do not
        cross VM filesystem mounts.
      '';
    };

    fileScope = lib.mkOption {
      type = lib.types.str;
      default = "/workspace,/home";
      description = ''
        Comma-separated path prefixes a file event has to be under to be
        exported. The agent's own default is `/workspace` alone; devbox widens
        it to include the box user's home, because an agent session that never
        leaves `~/.config` or `~/.cache` would otherwise look like a box doing
        nothing at all.

        `/home` rather than one user's directory is the safe default: on Lima
        the guest user has the *host's* uid and a `/home/<user>.guest` home its
        passwd entry does not name, so both spellings have to be covered.
        Provisioning overrides this with the same probed value it writes into
        the non-NixOS unit — two agents watching one box must not disagree
        about which paths produce file events, or the feed changes shape
        depending on who attached.
      '';
    };

    enableEbpf = lib.mkOption {
      type = lib.types.bool;
      # Local developer builds embed the portable proc+packet agent. Release
      # builds override this from the generated CO-RE artifact.
      default = false;
      description = ''
        Attach eBPF programs. Turning this off selects the degraded
        proc-polling path (§13). The independent packet tap still captures
        DNS and TLS; file-open fidelity is what is lost.
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
            "-packet=true"
            "-status-file" "/run/devbox/obsd-status.json"
            "-file-scope" (lib.escapeShellArg cfg.fileScope)
          ]
          ++ lib.optional (!cfg.enableEbpf) "-no-ebpf"
          ++ lib.optional cfg.noTransport "-no-transport"
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
        AmbientCapabilities = [
          "CAP_NET_RAW"
          "CAP_NET_ADMIN"
          "CAP_SYSLOG"
        ] ++ lib.optionals cfg.enableEbpf [
          "CAP_BPF"
          "CAP_PERFMON"
          "CAP_SYS_RESOURCE"
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
          # Loading the egress ruleset, which has nothing to do with eBPF: the
          # agent runs `nft -f -` at startup whenever `policy.json` carries
          # one, so an agent without this fails with EPERM, restarts, and fails
          # again — a restart loop on the *default* configuration, with the
          # box's firewall never coming back after a reboot.
          #
          # Round 32 made this set unconditional to stop the default running as
          # root with every capability, and left this entry on the eBPF side of
          # the split. Trading "too many capabilities" for "not enough to do
          # the job" is not a fix.
          "CAP_NET_ADMIN"
          # AF_PACKET is the payload source for DNS answers and TLS SNI in both
          # eBPF and proc modes.
          "CAP_NET_RAW"
        ] ++ lib.optionals cfg.enableEbpf [
          "CAP_BPF"
          "CAP_PERFMON"
          "CAP_SYS_RESOURCE"
        ];

        # The agent observes; it has no business writing anywhere except its
        # socket directory.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        RuntimeDirectory = "devbox";
        # The status contains only the pid and effective capture names. The
        # host control plane runs as the ordinary guest user, so it must be
        # able to read this file without acquiring root.
        RuntimeDirectoryMode = "0755";

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
