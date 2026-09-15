{
  description = "NNTP Proxy - A round-robin NNTP proxy server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
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

      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

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
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            # Only include Apple targets on macOS hosts where they can be built
            "x86_64-apple-darwin"
            "aarch64-apple-darwin"
          ]
          ++ pkgs.lib.optionals (!pkgs.stdenv.hostPlatform.isDarwin) [
            # Add additional Windows target for Linux hosts
            "aarch64-pc-windows-msvc"
          ];
      };

      # Basic development dependencies (no cross-compilation pollution)
      basicNativeBuildInputs = with pkgs;
        [
          rustToolchain

          # Code quality & linting
          cargo-deny
          cargo-audit
          cargo-hack
          cargo-shear
          cargo-semver-checks
          cargo-vet
          typos
          zizmor
          shellcheck
          actionlint

          # Testing & coverage
          cargo-tarpaulin
          cargo-nextest
          cargo-mutants
          cargo-careful

          # Build & dependencies
          cargo-outdated
          cargo-bloat

          # Utilities
          tokei
          gh

          # Performance profiling
          cargo-flamegraph

        ]
        ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
          perf
          heaptrack
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
        bashInteractive
      ];

      # Map system to Rust target triple env var prefix
      cargoTargetEnvPrefix =
        if system == "x86_64-linux" then "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU"
        else if system == "aarch64-linux" then "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU"
        else if system == "x86_64-darwin" then "CARGO_TARGET_X86_64_APPLE_DARWIN"
        else if system == "aarch64-darwin" then "CARGO_TARGET_AARCH64_APPLE_DARWIN"
        else throw "Unsupported system: ${system}";
      package = import ./nix/package.nix {
        inherit pkgs craneLib cargoToml;
      };

      gungraunRunner = pkgs.rustPlatform.buildRustPackage rec {
        pname = "gungraun-runner";
        version = "0.19.4";
        src = pkgs.fetchFromGitHub {
          owner = "gungraun";
          repo = "gungraun";
          rev = "v${version}";
          hash = "sha256-KWQ4wMNIdKY9FTmPd9ZdlSuCpQQBFhIKD2Ereo3JQaI=";
        };
        cargoHash = "sha256-+3toaUDLCmExC3EvNv1GEdUbHSBeShurp2Y+zvE/t0k=";
        cargoBuildFlags = ["-p" pname];
        cargoInstallFlags = ["-p" pname];
        doCheck = false;
      };
    in {
      apps.default =
        (flake-utils.lib.mkApp {
          drv = package;
        })
        // {
          meta = package.meta;
        };

      devShells.default = pkgs.mkShell {
        nativeBuildInputs = basicNativeBuildInputs ++ [gungraunRunner];
        buildInputs = devBuildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            # Linux: mold linker + native CPU optimizations
            export ${cargoTargetEnvPrefix}_LINKER="clang"
            export ${cargoTargetEnvPrefix}_RUSTFLAGS="-C link-arg=-fuse-ld=mold -C target-cpu=native"
          ''}

          ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
            # macOS: native CPU optimizations (no mold on macOS)
            export ${cargoTargetEnvPrefix}_RUSTFLAGS="-C target-cpu=native"
          ''}
        '';

        GH_PAGER = "cat";

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

      };

      packages.default = package;
      packages.nntp-proxy = package;
    })
    // {
      nixosModules.default = import ./nix/module.nix {inherit self;};
      nixosModules.nntp-proxy = self.nixosModules.default;
    };
}
