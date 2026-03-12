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
  sandbox = devboxConfig.sandbox or {};
  mountMode = sandbox.mount_mode or "overlay";
  isOverlay = mountMode == "overlay";
in {
  # ── Nixpkgs config ──────────────────────────────────
  # Allow unfree packages (claude-code, codex, etc.)
  nixpkgs.config.allowUnfree = true;

  # ── Packages ───────────────────────────────────────
  # Core sets (system + shell + tools + editor) are always installed.
  # Optional sets and language sets are conditional on devbox-state.toml.
  environment.systemPackages =
    devboxSets.system
    ++ devboxSets.shell
    ++ devboxSets.tools
    ++ devboxSets.editor
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
    ++ (lib.optionals (langs.ruby or false) devboxSets.lang_ruby);

  # ── Services ───────────────────────────────────────
  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      PermitRootLogin = "no";
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
  programs.zsh.enable = true;
  security.sudo.wheelNeedsPassword = lib.mkDefault false;

  # ── Environment ──────────────────────────────────
  environment.variables = {
    EDITOR = "nvim";
    VISUAL = "nvim";
  };

  # ── User configuration ────────────────────────────
  # Lima creates the user automatically; we declare it here so NixOS
  # manages the shell and group memberships properly.
  users.users.${username} = {
    isNormalUser = true;
    shell = lib.mkForce pkgs.zsh;
    extraGroups = lib.mkAfter ([ "wheel" ] ++ lib.optionals (sets.container or false) [ "docker" ]);
  };

  # ── OverlayFS Workspace Mount ─────────────────────────
  # In overlay mode, /mnt/host is the read-only host mount from Lima.
  # We overlay it at /workspace with a writable upper layer.
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
