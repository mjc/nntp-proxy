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
  }: let
    cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    rustToolchainToml = builtins.fromTOML (builtins.readFile ./rust-toolchain.toml);
    rustVersion = rustToolchainToml.toolchain.channel;
  in
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
        extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview"];
      };

      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      # Nightly toolchain for tools that require it (cargo-udeps)
      rustNightlyForUdeps = pkgs.rust-bin.nightly.latest.default;

      # Wrapper for cargo-udeps that uses nightly
      cargo-udeps-wrapped = pkgs.writeShellScriptBin "cargo-udeps" ''
        export RUSTC="${rustNightlyForUdeps}/bin/rustc"
        export CARGO="${rustNightlyForUdeps}/bin/cargo"
        exec ${pkgs.cargo-udeps}/bin/cargo-udeps "$@"
      '';

      # Stable toolchain with all cross-compilation targets for releases
      rustCrossToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
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
          cmake # Required for zlib-ng feature in flate2

          # Code quality & linting
          cargo-deny
          cargo-audit
          shellcheck

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

          # Build acceleration
          sccache # Build cache
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          perf
          cargo-llvm-cov
          mold # Fast linker (Linux only)
        ];

      # Cross-compilation tools (separate to avoid environment pollution)
      crossCompilationTools = with pkgs; [
        rustCrossToolchain # For cross-compilation builds
        cargo-zigbuild
        zig
        cmake
        nasm
        # Build script dependencies
        jq # JSON parsing for version detection
        zip # Windows release archives
        # tar is already available in most shells
        # Windows cross-compilation
        pkgsCross.mingwW64.buildPackages.binutils
      ];

      devBuildInputs = with pkgs; [
        openssl
        zlib
        bashInteractive
      ];

      packageNativeBuildInputs = with pkgs; [
        pkg-config
        cmake
      ]
      ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
        clang
        mold
      ];

      packageBuildInputs = with pkgs; [
        openssl
        zlib
      ];

      # Map system to Rust target triple env var prefix
      cargoTargetEnvPrefix =
        if system == "x86_64-linux" then "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU"
        else if system == "aarch64-linux" then "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU"
        else if system == "x86_64-darwin" then "CARGO_TARGET_X86_64_APPLE_DARWIN"
        else if system == "aarch64-darwin" then "CARGO_TARGET_AARCH64_APPLE_DARWIN"
        else throw "Unsupported system: ${system}";

      package = rustPlatform.buildRustPackage ({
        pname = cargoToml.package.name;
        version = cargoToml.package.version;
        src = ./.;

        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        nativeBuildInputs = packageNativeBuildInputs;
        buildInputs = packageBuildInputs;
        cargoBuildFlags = ["--bin" "nntp-proxy"];

        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";

        meta = with pkgs.lib; {
          description = cargoToml.package.description;
          homepage = cargoToml.package.homepage;
          license = licenses.mit;
          mainProgram = cargoToml.package.name;
          platforms = platforms.all;
        };
      }
      // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
        # Match the dev-shell linker setup for packaged builds without forcing
        # non-portable CPU tuning into release artifacts.
        "${cargoTargetEnvPrefix}_LINKER" = "clang";
        "${cargoTargetEnvPrefix}_RUSTFLAGS" = "-C link-arg=-fuse-ld=mold";
      });
    in {
      apps.default = flake-utils.lib.mkApp {
        drv = package;
      };

      devShells.default = pkgs.mkShell {
        nativeBuildInputs = basicNativeBuildInputs;
        buildInputs = devBuildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig"

          # Build acceleration
          export RUSTC_WRAPPER="sccache"

          ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            # Linux: mold linker + native CPU optimizations
            export ${cargoTargetEnvPrefix}_LINKER="clang"
            export ${cargoTargetEnvPrefix}_RUSTFLAGS="-C link-arg=-fuse-ld=mold -C target-cpu=native"
          ''}

          ${pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
            # macOS: native CPU optimizations (no mold on macOS)
            export ${cargoTargetEnvPrefix}_RUSTFLAGS="-C target-cpu=native"
          ''}

          echo "🦀 Rust development environment loaded!"
          echo "   Rust version: $(rustc --version)"
          echo "   Cargo version: $(cargo --version)"
          echo "   OpenSSL version: ${pkgs.openssl.version}"
          echo ""
          echo "🔧 Cross-compilation: Use './scripts/build-release.sh <version>' with the pinned stable toolchain"
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
          echo "   shellcheck scripts/*.sh - Lint shell scripts"
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

        # tikv-jemalloc-sys builds jemalloc from source; its configure script
        # fails strerror_r detection when _FORTIFY_SOURCE is set at -O0 (NixOS default).
        hardeningDisable = ["fortify"];
      };

      # Cross-compilation shell with all the tooling
      # NOTE: We use cargo-zigbuild which uses Zig as the linker for cross-compilation.
      # We do NOT need pkgsCross.mingwW64 tools as they pollute CC/CXX environment variables
      # and cause conflicts with CMAKE builds. Zig handles all cross-compilation itself.
      devShells.cross = pkgs.mkShell {
        nativeBuildInputs = basicNativeBuildInputs ++ crossCompilationTools;
        buildInputs = devBuildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig"

          export PATH="${rustCrossToolchain}/bin:$PATH"
          export NNTP_PROXY_CROSS_SHELL=1

          # Ensure no CC/CXX pollution - cargo-zigbuild uses Zig as linker
          unset CC
          unset CXX
          unset AR
          unset RANLIB

          # Windows cross-compilation: provide mingw-w64 libraries for windows-link crate
          # windows-sys 0.61+ with windows-link needs actual Windows lib files to link against
          MINGW_LIBS="${pkgs.pkgsCross.mingwW64.windows.mingw_w64}/lib"
          if [ -d "''${MINGW_LIBS}" ] && ls "''${MINGW_LIBS}"/libkernel32.* >/dev/null 2>&1; then
            export LIBRARY_PATH="''${MINGW_LIBS}:''${LIBRARY_PATH:-}"
            # Pass mingw libs to rustflags so cargo-zigbuild's zig linker can find them
            export RUSTFLAGS="''${RUSTFLAGS:-} -C link-arg=-L''${MINGW_LIBS}"
          else
            echo "warning: mingw-w64 libraries not found in nixpkgs (expected at ''${MINGW_LIBS}); Windows cross-linking may fail" >&2
          fi

          echo "🦀 Cross-compilation environment loaded!"
          echo "   Rust toolchain: ${rustCrossToolchain}/bin/rustc"
          echo "   Using cargo-zigbuild with Zig as linker (no mingw pollution)"
          echo "   Windows libraries: ''${MINGW_LIBS}"
          echo ""
        '';

        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";
      };

      packages.default = package;
      packages.nntp-proxy = package;
    })
    // {
      nixosModules.default = import ./nix/module.nix {inherit self;};
      nixosModules.nntp-proxy = self.nixosModules.default;
    };
}
