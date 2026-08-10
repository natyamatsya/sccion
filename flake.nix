{
  description = "sccion development environment and package";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }: let
    cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    version = cargoToml.package.version;

    mkSccion = {
      final,
      pname,
      features ? [],
      description,
    }:
      final.rustPlatform.buildRustPackage {
        pname = pname;
        inherit version;

        src = ./.;

        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        nativeBuildInputs = [final.pkg-config];
        buildInputs = [final.openssl];

        cargoBuildFlags =
          if features != []
          then [
            "--features"
            (builtins.concatStringsSep "," features)
          ]
          else [];

        doCheck = false;

        meta = with final.lib; {
          description = description;
          homepage = "https://github.com/natyamatsya/sccion";
          changelog = "https://github.com/natyamatsya/sccion/releases/tag/v${version}";
          license = licenses.asl20;
          mainProgram = "sccion";
        };
      };
  in
    flake-utils.lib.eachSystem
    [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
      "i686-linux"
    ]
    (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [self.overlays.default];
        };
      in {
        packages = {
          default = pkgs.sccion;
          sccion = pkgs.sccion;
          sccion-dist = pkgs.sccion-dist;
        };

        devShells.default = pkgs.mkShell {
          name = "sccion-dev";

          buildInputs = with pkgs; [
            rustup
            openssl
            pkg-config
            gcc
          ];
        };

        formatter = pkgs.alejandra;
      }
    )
    // {
      overlays.default = final: prev: {
        sccion = mkSccion {
          inherit final;
          pname = "sccion";
          description = "Ccache with Cloud Storage";
        };
        sccion-dist = mkSccion {
          inherit final;
          pname = "sccion-dist";
          features = [
            "dist-client"
            "dist-server"
          ];
          description = "Ccache with Cloud Storage and Distributed Compilation";
        };
      };
    };
}
