# Devbox v3 — Standalone NixOS VM configuration (reference / custom image builds)
#
# NOTE: For Lima-based VMs, use nix/devbox-module.nix instead.
# This file is the standalone config used by `nix build` (via flake.nix)
# to build custom NixOS images with all tools pre-installed.
#
# The devbox-module.nix approach is preferred because it works with
# stock NixOS Lima images — no custom image build required.
{ config, pkgs, lib, ... }:

let
  devboxSets = import /etc/devbox/sets { inherit pkgs; };
  devboxConfig = builtins.fromTOML (builtins.readFile /etc/devbox/devbox-state.toml);
  username = devboxConfig.user.name or "dev";
in {
  system.stateVersion = "24.11";
  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  boot.loader.systemd-boot.enable = true;
  networking.hostName = "devbox";
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # ── User ───────────────────────────────────────────
  users.users.${username} = {
    isNormalUser = true;
    home = "/home/${username}";
    shell = pkgs.zsh;
    extraGroups = [ "wheel" "docker" "networkmanager" ];
  };
  security.sudo.wheelNeedsPassword = false;

  # ── Packages ───────────────────────────────────────
  environment.systemPackages =
    devboxSets.system
    ++ devboxSets.shell
    ++ devboxSets.tools
    ++ (lib.optionals (devboxConfig.sets.editor or true) devboxSets.editor)
    ++ (lib.optionals (devboxConfig.sets.git or true) devboxSets.git)
    ++ (lib.optionals (devboxConfig.sets.container or false) devboxSets.container)
    ++ (lib.optionals (devboxConfig.sets.network or false) devboxSets.network)
    ++ (lib.optionals (devboxConfig.sets.ai or false) devboxSets.ai)
    ++ (lib.optionals (devboxConfig.languages.go or false) devboxSets.lang_go)
    ++ (lib.optionals (devboxConfig.languages.rust or false) devboxSets.lang_rust)
    ++ (lib.optionals (devboxConfig.languages.python or false) devboxSets.lang_python)
    ++ (lib.optionals (devboxConfig.languages.node or false) devboxSets.lang_node)
    ++ (lib.optionals (devboxConfig.languages.java or false) devboxSets.lang_java)
    ++ (lib.optionals (devboxConfig.languages.ruby or false) devboxSets.lang_ruby);

  # ── Services ───────────────────────────────────────
  virtualisation.docker.enable = devboxConfig.sets.container or false;
  services.openssh.enable = true;
  services.tailscale.enable = devboxConfig.sets.network or false;

  # ── Shell ──────────────────────────────────────────
  programs.zsh.enable = true;

  # ── Nix Garbage Collection ─────────────────────────
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
}
