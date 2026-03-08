# Devbox v3 — Master flake for NixOS image building
{
  description = "Devbox v3 — NixOS-powered developer VM";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    home-manager = {
      url = "github:nix-community/home-manager/release-24.11";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixos-generators = {
      url = "github:nix-community/nixos-generators";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, home-manager, nixos-generators, ... }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    # NixOS configurations for image building
    nixosConfigurations.devbox = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        home-manager.nixosModules.home-manager
      ];
    };

    nixosConfigurations.devbox-aarch64 = nixpkgs.lib.nixosSystem {
      system = "aarch64-linux";
      modules = [
        ./configuration.nix
        home-manager.nixosModules.home-manager
      ];
    };

    # Image generators
    packages = forAllSystems (system: {
      # QCOW2 for Incus/QEMU
      qcow2 = nixos-generators.nixosGenerate {
        inherit system;
        format = "qcow2";
        modules = [
          ./configuration.nix
          home-manager.nixosModules.home-manager
        ];
      };

      # Raw image for Lima
      raw = nixos-generators.nixosGenerate {
        inherit system;
        format = "raw";
        modules = [
          ./configuration.nix
          home-manager.nixosModules.home-manager
        ];
      };
    });
  };
}
