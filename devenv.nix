{ pkgs, lib, ... }:

let
  linux = pkgs.stdenv.hostPlatform.isLinux;
in
{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
    clangLinker.enable = false;
    mold.enable = linux;
  };

  packages = with pkgs;
    [
      pkg-config
      cmake
      openssl
      zlib
      cargo-deny
      cargo-audit
      cargo-hack
      cargo-shear
      cargo-vet
      cargo-nextest
      cargo-mutants
      cargo-careful
      cargo-outdated
      cargo-llvm-cov
      shellcheck
      actionlint
      zizmor
      typos
      jq
    ]
    ++ lib.optionals linux [ perf heaptrack mold ];

  env = {
    CARGO_TERM_COLOR = "always";
    GH_PAGER = "cat";
    OPENSSL_DIR = "${pkgs.openssl.dev}";
    OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
    PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig";
    NIX_HARDENING_DISABLE = "fortify";
  };

  # Install the hook when the shell activates; the hook itself runs only for
  # commits, never as part of `direnv allow` or ordinary shell entry.
  git-hooks.hooks."nntp-proxy-quality-fast" = {
    enable = true;
    name = "nntp-proxy quality-fast";
    entry = "scripts/quality-fast.sh";
    language = "system";
    pass_filenames = false;
    always_run = true;
    stages = [ "commit" ];
  };

  enterShell = ''
    # Do not inherit a host-wide CMake launcher: it can rewrite Clang's
    # target flags and break native build scripts such as zlib-ng.
    unset CMAKE_C_COMPILER_LAUNCHER CMAKE_CXX_COMPILER_LAUNCHER
    export RUSTFLAGS="''${RUSTFLAGS:-} -C target-cpu=native"
  '';
}
