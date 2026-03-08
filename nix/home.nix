# Devbox v3 — Home-manager configuration for the `dev` user
# This file is deployed to /etc/devbox/home.nix inside the VM.
{ pkgs, ... }:
{
  programs.zsh = {
    enable = true;
    autosuggestion.enable = true;
    syntaxHighlighting.enable = true;
    shellAliases = {
      ls = "eza --icons";
      cat = "bat --paging=never";
      top = "htop";
      diff = "delta";
    };
    initExtra = ''
      # Devbox identity
      export DEVBOX_NAME="''${DEVBOX_NAME:-devbox}"
      export DEVBOX_RUNTIME="''${DEVBOX_RUNTIME:-unknown}"

      # Modern tool aliases (interactive only, won't break scripts)
      if [[ $- == *i* ]]; then
        alias f='fd'
        alias g='rg'
      fi
    '';
  };

  programs.starship = {
    enable = true;
    settings = {
      format = "$custom$directory$git_branch$git_status$golang$rust$python$nodejs$line_break$character";
      custom.devbox = {
        command = "echo $DEVBOX_NAME";
        when = "true";
        format = "[$output]($style) ";
        style = "bold blue";
      };
    };
  };

  programs.zoxide = {
    enable = true;
    enableZshIntegration = true;
  };

  programs.fzf = {
    enable = true;
    enableZshIntegration = true;
  };

  programs.git = {
    enable = true;
    delta.enable = true;
    extraConfig = {
      init.defaultBranch = "main";
      push.autoSetupRemote = true;
    };
  };

  programs.zellij = {
    enable = true;
  };
}
