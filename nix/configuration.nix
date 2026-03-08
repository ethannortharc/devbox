# Devbox v3 — NixOS VM base configuration
# This file is deployed to /etc/nixos/configuration.nix inside the VM.
{ config, pkgs, lib, ... }:

let
  devboxSets = import /etc/devbox/sets { inherit pkgs; };
  devboxConfig = builtins.fromTOML (builtins.readFile /etc/devbox/devbox-state.toml);
in {
  system.stateVersion = "24.11";
  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  boot.loader.systemd-boot.enable = true;
  networking.hostName = "devbox";
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # ── User ───────────────────────────────────────────
  users.users.dev = {
    isNormalUser = true;
    home = "/home/dev";
    shell = pkgs.zsh;
    extraGroups = [ "wheel" "docker" "networkmanager" ];
  };
  security.sudo.wheelNeedsPassword = false;

  # ── Packages ───────────────────────────────────────
  # Core sets (system + shell + tools) are always installed.
  # Optional sets and language sets are conditional on devbox-state.toml.
  environment.systemPackages =
    devboxSets.system
    ++ devboxSets.shell
    ++ devboxSets.tools
    ++ (lib.optionals devboxConfig.sets.editor devboxSets.editor)
    ++ (lib.optionals devboxConfig.sets.git devboxSets.git)
    ++ (lib.optionals devboxConfig.sets.container devboxSets.container)
    ++ (lib.optionals devboxConfig.sets.network devboxSets.network)
    ++ (lib.optionals devboxConfig.sets.ai devboxSets.ai)
    ++ (lib.optionals devboxConfig.languages.go devboxSets.lang_go)
    ++ (lib.optionals devboxConfig.languages.rust devboxSets.lang_rust)
    ++ (lib.optionals devboxConfig.languages.python devboxSets.lang_python)
    ++ (lib.optionals devboxConfig.languages.node devboxSets.lang_node)
    ++ (lib.optionals devboxConfig.languages.java devboxSets.lang_java)
    ++ (lib.optionals devboxConfig.languages.ruby devboxSets.lang_ruby);

  # ── Services ───────────────────────────────────────
  virtualisation.docker.enable = devboxConfig.sets.container;
  services.openssh.enable = true;
  services.tailscale.enable = devboxConfig.sets.network;

  # ── Shell Environment (home-manager) ───────────────
  home-manager.users.dev = import /etc/devbox/home.nix { inherit pkgs; };

  # ── Nix Garbage Collection ─────────────────────────
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
}
