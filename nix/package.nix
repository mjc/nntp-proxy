{
  pkgs,
  cargoToml,
  rustVersion,
}:
let
  rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
    extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview"];
  };

  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };

  cargoTargetEnvPrefix =
    if pkgs.stdenv.hostPlatform.system == "x86_64-linux" then "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU"
    else if pkgs.stdenv.hostPlatform.system == "aarch64-linux" then "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU"
    else if pkgs.stdenv.hostPlatform.system == "x86_64-darwin" then "CARGO_TARGET_X86_64_APPLE_DARWIN"
    else if pkgs.stdenv.hostPlatform.system == "aarch64-darwin" then "CARGO_TARGET_AARCH64_APPLE_DARWIN"
    else throw "Unsupported system: ${pkgs.stdenv.hostPlatform.system}";
in
  rustPlatform.buildRustPackage (
    {
      pname = cargoToml.package.name;
      version = cargoToml.package.version;
      src = ../.;

      cargoLock = {
        lockFile = ../Cargo.lock;
      };

      nativeBuildInputs = with pkgs; [
        pkg-config
        cmake
      ]
      ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
        clang
        mold
      ];

      buildInputs = with pkgs; [
        openssl
        zlib
      ];

      cargoBuildFlags = ["--bin" "nntp-proxy"];

      postInstall = ''
        cat > "$out/bin/nntp-proxy-tui" <<'EOF'
        #!/bin/sh
        attach_addr="127.0.0.1:8120"
        if [ "$#" -gt 0 ] && [ "''${1#-}" = "$1" ]; then
          attach_addr="$1"
          shift
        fi
        exec "@out@/bin/nntp-proxy" --ui tui --tui-attach "$attach_addr" "$@"
        EOF
        substituteInPlace "$out/bin/nntp-proxy-tui" --replace-fail "@out@" "$out"
        chmod +x "$out/bin/nntp-proxy-tui"
      '';

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
    }
  )
