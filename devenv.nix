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
    ++ lib.optionals linux [ perf heaptrack ];

  env = {
    CARGO_TERM_COLOR = "always";
    GH_PAGER = "cat";
    NIX_HARDENING_DISABLE = "fortify";
  };

  # Install the hook when the shell activates; the hook itself runs only for
  # commits, never during shell activation.
  git-hooks.hooks."nntp-proxy-quality-fast" = {
    enable = true;
    name = "nntp-proxy quality-fast";
    entry = "devenv shell scripts/quality-fast.sh";
    language = "system";
    pass_filenames = false;
    always_run = true;
    stages = [ "commit" ];
  };

  # `git-hooks:run` belongs to `devenv test`, not ordinary shell entry.
  tasks."devenv:git-hooks:run".before = lib.mkForce [ ];

  enterShell = ''
    export RUSTFLAGS="''${RUSTFLAGS:-} -C target-cpu=native"
  '';
}
