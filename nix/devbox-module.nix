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
in {
  # ── Packages ───────────────────────────────────────
  # Core sets (system + shell + tools) are always installed.
  # Optional sets and language sets are conditional on devbox-state.toml.
  environment.systemPackages =
    devboxSets.system
    ++ devboxSets.shell
    ++ devboxSets.tools
    ++ (lib.optionals (sets.editor or true) devboxSets.editor)
    ++ (lib.optionals (sets.git or true) devboxSets.git)
    ++ (lib.optionals (sets.container or false) devboxSets.container)
    ++ (lib.optionals (sets.network or false) devboxSets.network)
    ++ (lib.optionals (sets.ai or false) devboxSets.ai)
    ++ (lib.optionals (langs.go or false) devboxSets.lang_go)
    ++ (lib.optionals (langs.rust or false) devboxSets.lang_rust)
    ++ (lib.optionals (langs.python or false) devboxSets.lang_python)
    ++ (lib.optionals (langs.node or false) devboxSets.lang_node)
    ++ (lib.optionals (langs.java or false) devboxSets.lang_java)
    ++ (lib.optionals (langs.ruby or false) devboxSets.lang_ruby);

  # ── Services ───────────────────────────────────────
  virtualisation.docker.enable = lib.mkDefault (sets.container or false);
  services.tailscale.enable = lib.mkDefault (sets.network or false);

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
    extraGroups = lib.mkAfter [ "wheel" "docker" ];
  };

  # ── Nix Garbage Collection ─────────────────────────
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 14d";
  };
}
