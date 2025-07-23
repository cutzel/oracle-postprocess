{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };
  outputs = {
    self,
    nixpkgs,
  }: let
    supportedSystems = ["x86_64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    nixpkgsFor = forAllSystems (system: import nixpkgs {inherit system;});
  in {
    formatter = forAllSystems (system: let
      pkgs = nixpkgsFor.${system};
    in
      pkgs.writeShellScriptBin "alejandra-formatter" ''
        ${pkgs.alejandra}/bin/alejandra .
      '');

    packages = forAllSystems (
      system: let
        pkgs = nixpkgsFor.${system};
      in {
        oracle-postprocess = pkgs.buildRustPackage {
          name = "oracle-postprocess";
          src = ./.;
        };
      }
    );

    devShells."x86_64-linux".default = let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
      };
    in
      with pkgs;
        mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            pkg-config
            openssl
          ];
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
  };
}
