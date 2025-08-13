{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
    shellHook = ''
        export BINDGEN_EXTRA_CLANG_ARGS="\
          $(< ${pkgs.clang}/nix-support/cc-cflags) \
          -I${pkgs.glibc.dev}/include \
          -I${pkgs.openssl.dev}/include \
        "
        export CFLAGS="-I${pkgs.openssl.dev}/include -O2"
        export LIBCLANG_PATH="${pkgs.libclang.lib}/lib"
    '';
}