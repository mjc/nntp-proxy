{
  description = "NNTP Proxy - A round-robin NNTP proxy server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["rust-src" "rust-analyzer"];
      };

      nativeBuildInputs = with pkgs; [
        rustToolchain
        pkg-config

        # Performance profiling
        cargo-flamegraph
        linuxPackages.perf

        # Code quality & linting
        cargo-deny
        cargo-audit

        # Testing & coverage
        cargo-tarpaulin
        cargo-nextest
        cargo-mutants

        # Build & dependencies
        cargo-outdated
        cargo-bloat

        # Utilities
        gh
      ];

      buildInputs = with pkgs;
        [
          openssl
          zlib
        ]
        ++ lib.optionals stdenv.isDarwin [
          darwin.apple_sdk.frameworks.Security
          darwin.apple_sdk.frameworks.SystemConfiguration
        ];
    in {
      devShells.default = pkgs.mkShell {
        inherit nativeBuildInputs buildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig"

          echo "🦀 Rust development environment loaded!"
          echo "   Rust version: $(rustc --version)"
          echo "   Cargo version: $(cargo --version)"
          echo "   OpenSSL version: ${pkgs.openssl.version}"
          echo ""
          echo "📦 Available commands:"
          echo "   cargo build       - Build the project"
          echo "   cargo run         - Run the NNTP proxy"
          echo "   cargo test        - Run tests"
          echo "   cargo clippy      - Run linter"
          echo "   cargo fmt         - Format code"
          echo ""
          echo "🔍 Code quality:"
          echo "   cargo deny check  - Check dependencies for security/licenses"
          echo "   cargo audit       - Check for security vulnerabilities"
          echo ""
          echo "🧪 Testing & coverage:"
          echo "   cargo nextest run - Fast test runner"
          echo "   cargo tarpaulin   - Code coverage analysis"
          echo "   cargo mutants     - Mutation testing"
          echo ""
          echo "⚡ Performance:"
          echo "   cargo flamegraph  - Generate performance flamegraph"
          echo "   cargo bench       - Run benchmarks"
          echo "   perf              - Linux performance analysis tools"
          echo ""
          echo "📊 Dependencies:"
          echo "   cargo outdated    - Check for outdated dependencies"
          echo "   cargo tree        - Visualize dependency tree"
          echo "   cargo bloat       - Find what takes up space in binary"
          echo ""
        '';

        # Environment variables for building with OpenSSL
        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";
      };

      packages.default = pkgs.rustPlatform.buildRustPackage {
        pname = "nntp-proxy";
        version = "0.1.0";
        src = ./.;

        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        inherit nativeBuildInputs buildInputs;

        # Environment variables for building with OpenSSL
        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";

        meta = with pkgs.lib; {
          description = "A round-robin NNTP proxy server";
          license = licenses.mit;
          platforms = platforms.all;
        };
      };
    });
}
