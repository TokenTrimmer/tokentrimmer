{
  description = "TokenTrimmer dev shell — locked Rust toolchain, Node, tree-sitter parsers.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable."1.83.0".default.override {
          extensions = [ "rust-src" "rustfmt" "clippy" "rust-analyzer" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            cargo-deny
            cargo-audit
            cargo-nextest
            cargo-watch

            nodejs_20
            pnpm

            postgresql_16
            redis

            gh
            jq
            yq-go
            ripgrep
            fd
            git
            gnumake

            pkg-config
            openssl
          ];

          shellHook = ''
            echo "TokenTrimmer dev shell (rust $(rustc --version | awk '{print $2}'), node $(node --version))"
            echo "Run 'docker compose -f docker-compose.dev.yml up' for runtime services."
          '';

          RUST_BACKTRACE = "1";
        };
      });
}
