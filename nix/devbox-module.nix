# Devbox v3 — NixOS module for developer tools
# This file is pushed into the VM at /etc/devbox/devbox-module.nix
# and imported into the VM's existing /etc/nixos/configuration.nix.
#
# It reads /etc/devbox/devbox-state.toml to determine which package
# sets and languages to install, then adds them to the system.
{ config, pkgs, lib, ... }:

let
  devboxSets = import /etc/devbox/sets { inherit pkgs; };

  stateFile = /etc/devbox/devbox-state.toml;
  hasState = builtins.pathExists stateFile;
  devboxConfig = if hasState
    then builtins.fromTOML (builtins.readFile stateFile)
    else { sets = {}; languages = {}; };

  sets = devboxConfig.sets or {};
  langs = devboxConfig.languages or {};
  username = devboxConfig.user.name or "dev";
  # The home the box's login shell actually uses, when devbox was able to ask
  # the box for it. Absent on a box provisioned before devbox recorded it.
  userHome = devboxConfig.user.home or null;
  sandbox = devboxConfig.sandbox or {};
  mountMode = sandbox.mount_mode or "overlay";
  isOverlay = mountMode == "overlay";
  hasEditor = sets.editor or true;
  hasShell = sets.shell or true;

  # Ad-hoc packages from the Sets checklist's free-text field.
  #
  # `python312Packages.ipython` is a *path*, and a bare dotted TOML key is a
  # nested table — so `attrNames` yields `python312Packages` and the lookup
  # returns a whole package set rather than the leaf derivation. Walking the
  # table recursively recovers the full path, and `attrByPath` resolves it.
  #
  # An attribute that does not exist is skipped rather than failing the whole
  # rebuild: one stale name in the free-text field should not brick the box.
  # A key may arrive either way: `python312Packages.ipython` unquoted is a
  # nested table, quoted it is one literal key containing a dot. Both mean the
  # same attribute path, so both are split into one.
  flattenPaths = prefix: attrs:
    lib.concatLists (lib.mapAttrsToList (name: value:
      let path = prefix ++ (lib.splitString "." name);
      in if builtins.isAttrs value then flattenPaths path value else [ path ]
    ) attrs);

  customPaths = flattenPaths [ ] (devboxConfig.custom_packages or { });
  customPackages = builtins.filter (p: p != null)
    (map (path: lib.attrByPath path null pkgs) customPaths);
in {
  # ── Nixpkgs config ──────────────────────────────────
  # Allow unfree packages (claude-code, codex, etc.)
  nixpkgs.config.allowUnfree = true;

  # ── Packages ───────────────────────────────────────
  # Only `system` is locked (ADR-0012). Forcing shell/tools/editor on here
  # would make unchecking them in the console a silent no-op — the box would
  # report the new selection and install the old one. Their defaults stay
  # `true`, so a box provisioned before this change is unaffected.
  environment.systemPackages =
    devboxSets.system
    ++ (lib.optionals (sets.shell or true) devboxSets.shell)
    ++ (lib.optionals (sets.tools or true) devboxSets.tools)
    ++ (lib.optionals (sets.editor or true) devboxSets.editor)
    ++ (lib.optionals (sets.git or true) devboxSets.git)
    ++ (lib.optionals (sets.container or false) devboxSets.container)
    ++ (lib.optionals (sets.network or false) devboxSets.network)
    ++ (lib.optionals (sets.ai_code or true) devboxSets.ai_code)
    ++ (lib.optionals (sets.ai_infra or false) devboxSets.ai_infra)
    ++ (lib.optionals (langs.go or false) devboxSets.lang_go)
    ++ (lib.optionals (langs.rust or false) devboxSets.lang_rust)
    ++ (lib.optionals (langs.python or false) devboxSets.lang_python)
    ++ (lib.optionals (langs.node or false) devboxSets.lang_node)
    ++ (lib.optionals (langs.java or false) devboxSets.lang_java)
    ++ (lib.optionals (langs.ruby or false) devboxSets.lang_ruby)
    # Ad-hoc packages from the Sets checklist's free-text field. Every name is
    # validated host-side against a strict attribute-path pattern before it is
    # written here, and an attribute that does not exist is skipped rather
    # than failing the whole rebuild.
    ++ (lib.optional (!hasEditor) pkgs.nano)
    ++ customPackages;

  # ── Services ───────────────────────────────────────
  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      PermitRootLogin = "no";
      # `devbox code` hands the editor a Remote SSH target and then gets out of
      # the way, so there is no guest process for devbox to start with an
      # environment. The credential broker's variables travel as ssh
      # environment instead: the host's `Host` block sends them with `SetEnv`,
      # and this is the half that lets sshd through. Nothing here is a
      # credential and nothing here is written into the box — the box token
      # arrives per connection and lives only in that session.
      #
      # sshd's default AcceptEnv is empty, so without this the variables are
      # dropped silently and the editor's terminal has no broker at all.
      # Kept in step with `broker::SSH_ACCEPT_ENV` by a test.
      AcceptEnv = "DEVBOX_BROKER_URL DEVBOX_BROKER_TOKEN DEVBOX_BROKER_GITHUB_URL DEVBOX_RUN_ID DEVBOX_SECRET_*_URL ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN OPENAI_BASE_URL OPENAI_API_KEY";
    };
  };
  virtualisation.docker.enable = lib.mkDefault (sets.container or false);
  services.tailscale.enable = lib.mkDefault (sets.network or false);

  # ── Networking ──────────────────────────────────────
  # Enable DHCP on all interfaces so the VM gets an IP regardless of
  # interface name (e.g. after launching from a cached image where the
  # NIC name may have changed).
  networking.useDHCP = lib.mkDefault true;

  # ── Incus/LXD Agent ─────────────────────────────────
  # Required for `incus exec` to work after nixos-rebuild.
  # This NixOS module installs the agent service with proper udev rules,
  # 9p mount setup, and critically: restartIfChanged=false / stopIfChanged=false
  # so nixos-rebuild doesn't kill the agent mid-switch (which would drop our
  # incus exec connection).
  virtualisation.incus.agent.enable = true;

  # ── Dynamic linker compat ─────────────────────────
  # Required for VS Code Server, Cursor, and other dynamically linked
  # binaries that expect a standard FHS layout (ld-linux, libc, etc.).
  programs.nix-ld.enable = true;

  # ── Shell ──────────────────────────────────────────
  # Only when the shell set is installed. Enabling zsh and then forcing it as
  # the login shell on a box where the user unchecked `shell` left them with a
  # login shell that does not exist.
  programs.zsh.enable = lib.mkDefault hasShell;
  security.sudo.wheelNeedsPassword = lib.mkDefault false;

  # ── Environment ──────────────────────────────────
  environment.variables = {
    # Only when the editor set is actually installed. Exporting EDITOR=nvim on
    # a box where the user unchecked `editor` makes `git commit` fail with a
    # missing editor rather than falling back to something that exists.
    # `vi` is not guaranteed either — the system set has no editor at all. The
    # fallback has to be something the closure really contains, and `nano` is
    # in `pkgs` unconditionally, so it is added alongside when the editor set
    # is off rather than named on faith.
    EDITOR = if hasEditor then "nvim" else "nano";
    VISUAL = if hasEditor then "nvim" else "nano";
  };

  # ── User configuration ────────────────────────────
  # Lima creates the user automatically; we declare it here so NixOS
  # manages the shell and group memberships properly.
  #
  # `home` is declared too, and that is not cosmetic. `isNormalUser` defaults
  # it to `/home/${username}`, so a box whose user Lima created somewhere else
  # had its passwd entry silently re-homed by the first `nixos-rebuild`. On a
  # Mac, Lima's cloud-init uses `/home/<name>.guest` — the host's own home is
  # mounted into the guest and the two names would collide — and it writes the
  # box's `authorized_keys` there. Re-homing left sshd resolving the home to a
  # directory with no keys in it: every ssh connection opened after the first
  # rebuild was refused, and the box stayed reachable only through the shared
  # connection `limactl` had already authenticated, whose cached passwd entry
  # still named the old home. That is also why the two disagreed about `$HOME`.
  #
  # devbox asks the box where its login shell lives and records the answer in
  # `devbox-state.toml`; this keeps NixOS from overruling it. Absent on a box
  # provisioned before that was recorded, and then the old default applies —
  # `agent_sync` is what brings those boxes back.
  users.users.${username} = {
    isNormalUser = true;
    shell = lib.mkForce (if hasShell then pkgs.zsh else pkgs.bashInteractive);
    extraGroups = lib.mkAfter ([ "wheel" ] ++ lib.optionals (sets.container or false) [ "docker" ]);
  } // lib.optionalAttrs (userHome != null) { home = userHome; };

  # ── OverlayFS Workspace Mount ─────────────────────────
  # In overlay mode, /mnt/host is the read-only host mount from Lima.
  # We overlay it at /workspace with a writable upper layer.
  #
  # `nofail` would have been worth having — without it an overlay that cannot
  # be assembled takes `local-fs.target` down and the box comes up in emergency
  # mode with no sshd. But it is not worth reaching a box that can no longer be
  # repaired, and it was never the fix for anything: the boxes that were being
  # lost were lost to a stale cidata UUID, which is fixed in the generated
  # `configuration.nix` and needs no option change here.
  fileSystems."/workspace" = lib.mkIf isOverlay {
    device = "overlay";
    fsType = "overlay";
    options = [
      "lowerdir=/mnt/host"
      "upperdir=/var/devbox/overlay/upper"
      "workdir=/var/devbox/overlay/work"
    ];
    depends = [ "/mnt/host" ];
  };

  # Create overlay directories on boot (owned by user so writes land as the user)
  systemd.tmpfiles.rules = lib.mkIf isOverlay [
    "d /var/devbox/overlay/upper 0755 ${username} users -"
    "d /var/devbox/overlay/work 0755 root root -"
    "d /mnt/host 0755 root root -"
    "d /workspace 0755 ${username} users -"
  ];

  # Fix /workspace ownership after overlay mount (overlay resets to root)
  systemd.services.devbox-workspace-perms = lib.mkIf isOverlay {
    description = "Set /workspace ownership for devbox user";
    after = [ "local-fs.target" ];
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${pkgs.coreutils}/bin/chown ${username}:users /workspace /var/devbox/overlay/upper";
      RemainAfterExit = true;
    };
  };

  # ── Nix Garbage Collection ─────────────────────────
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
}
