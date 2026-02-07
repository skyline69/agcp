{
  description = "AGCP - Lightweight Rust proxy translating Anthropic Claude API to Google Cloud Code API";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.stable."1.93.0".minimal;
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          meta = with pkgs.lib; {
            description = "Lightweight Rust proxy translating Anthropic Claude API to Google Cloud Code API";
            homepage = "https://github.com/skyline69/agcp";
            license = licenses.mit;
            maintainers = [ ];
            mainProgram = "agcp";
          };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.pkg-config
          ];
        };
      }
    );
}
