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
        extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview"];
      };

      # Nightly toolchain for tools that require it (cargo-udeps)
      rustNightlyForUdeps = pkgs.rust-bin.nightly.latest.default;

      # Wrapper for cargo-udeps that uses nightly
      cargo-udeps-wrapped = pkgs.writeShellScriptBin "cargo-udeps" ''
        export RUSTC="${rustNightlyForUdeps}/bin/rustc"
        export CARGO="${rustNightlyForUdeps}/bin/cargo"
        exec ${pkgs.cargo-udeps}/bin/cargo-udeps "$@"
      '';

      # Nightly toolchain with all cross-compilation targets for releases
      rustNightlyToolchain = pkgs.rust-bin.nightly.latest.default.override {
        extensions = ["rust-src"];
        targets =
          [
            # Always include these common targets
            "x86_64-unknown-linux-gnu"
            "aarch64-unknown-linux-gnu"
            "x86_64-pc-windows-gnu"
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # Only include Apple targets on macOS hosts where they can be built
            "x86_64-apple-darwin"
            "aarch64-apple-darwin"
          ]
          ++ pkgs.lib.optionals (!pkgs.stdenv.isDarwin) [
            # Add additional Windows target for Linux hosts
            "aarch64-pc-windows-msvc"
          ];
      };

      # Basic development dependencies (no cross-compilation pollution)
      basicNativeBuildInputs = with pkgs;
        [
          rustToolchain
          pkg-config

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
          cargo-udeps-wrapped

          # Utilities
          tokei
          gh

          # Performance profiling
          cargo-flamegraph
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          perf
          cargo-llvm-cov
        ];

      # Cross-compilation tools (separate to avoid environment pollution)
      crossCompilationTools = with pkgs; [
        rustNightlyToolchain # For cross-compilation builds
        cargo-zigbuild
        zig
        cmake
        nasm
        # Build script dependencies
        jq # JSON parsing for version detection
        zip # Windows release archives
        # tar is already available in most shells
        # Windows cross-compilation - we need binutils (dlltool) but not the full mingw CC/CXX
        pkgsCross.mingwW64.buildPackages.binutils
      ];

      buildInputs = with pkgs; [
        openssl
        zlib
      ];
    in {
      devShells.default = pkgs.mkShell {
        nativeBuildInputs = basicNativeBuildInputs;
        inherit buildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig"

          echo "🦀 Rust development environment loaded (NIGHTLY)!"
          echo "   Rust version: $(rustc --version)"
          echo "   Cargo version: $(cargo --version)"
          echo "   OpenSSL version: ${pkgs.openssl.version}"
          echo ""
          echo "🔧 Cross-compilation: Use './scripts/build-release.sh <version>' with nightly toolchain"
          echo ""
          echo "📦 Available commands:"
          echo "   cargo build       - Build the project"
          echo "   cargo run         - Run the NNTP proxy"
          echo "   cargo test        - Run tests"
          echo "   cargo clippy      - Run linter"
          echo "   cargo fmt         - Format code"
          echo "   ./scripts/build-release.sh <version> - Build all release binaries"
          echo ""
          echo "🔍 Code quality:"
          echo "   cargo deny check  - Check dependencies for security/licenses"
          echo "   cargo audit       - Check for security vulnerabilities"
          echo ""
          echo "🧪 Testing & coverage:"
          echo "   cargo nextest run - Fast test runner"
          echo "   cargo tarpaulin   - Code coverage analysis"
          echo "   cargo mutants     - Mutation testing"
          echo "   cargo llvm-cov    - LLVM-based code coverage"
          echo ""
          echo "⚡ Performance:"
          echo "   cargo flamegraph  - Generate performance flamegraph"
          echo "   cargo bench       - Run benchmarks"
          ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            echo "   perf              - Linux performance analysis tools"
          ''}
          echo ""
          echo "📊 Dependencies:"
          echo "   cargo outdated    - Check for outdated dependencies"
          echo "   cargo tree        - Visualize dependency tree"
          echo "   cargo bloat       - Find what takes up space in binary"
          echo ""
        '';

        GH_PAGER = "cat";

        # Environment variables for building with OpenSSL
        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";
      };

      # Cross-compilation shell with all the tooling
      # NOTE: We use cargo-zigbuild which uses Zig as the linker for cross-compilation.
      # We do NOT need pkgsCross.mingwW64 tools as they pollute CC/CXX environment variables
      # and cause conflicts with CMAKE builds. Zig handles all cross-compilation itself.
      devShells.cross = pkgs.mkShell {
        nativeBuildInputs = basicNativeBuildInputs ++ crossCompilationTools;
        inherit buildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig"

          # Add nightly toolchain to PATH for cross-compilation
          export PATH="${rustNightlyToolchain}/bin:$PATH"
          export RUSTUP_TOOLCHAIN="nightly"

          # Ensure no CC/CXX pollution - cargo-zigbuild uses Zig as linker
          unset CC
          unset CXX
          unset AR
          unset RANLIB

          echo "🦀 Cross-compilation environment loaded!"
          echo "   Nightly toolchain: ${rustNightlyToolchain}/bin/rustc"
          echo "   Using cargo-zigbuild with Zig as linker (no mingw pollution)"
          echo ""
        '';

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

        nativeBuildInputs = basicNativeBuildInputs;
        inherit buildInputs;

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
