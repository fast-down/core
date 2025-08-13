# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      stdenv = pkgs.stdenv;
      lib = pkgs.lib;
      fhs = pkgs.buildFHSEnv {
        name = "fhs-shell";
        targetPkgs = pkgs: with pkgs; [gcc glibc.dev gcc glibc clang libclang openssl openssl.dev boringssl boringssl.dev nghttp2 nghttp3 nghttp3.dev ngtcp2 curl curl.dev];
      };
    in
      {
        devShells.${system}.default = fhs.env;
      };
}