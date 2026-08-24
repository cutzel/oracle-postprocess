{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
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

    packages = forAllSystems (system: let
      pkgs = nixpkgsFor.${system};
      craneLib = crane.mkLib pkgs;
      commonArgs = {
        pname = "oracle-postprocess";
        src = craneLib.cleanCargoSource ./.;
        strictDeps = true;

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
        ];
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      oracle-postprocess = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
        });
    in {
      default = oracle-postprocess;
      inherit oracle-postprocess;
    });

    devShells = forAllSystems (system: let
      pkgs = nixpkgsFor.${system};
    in {
      default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          rustc
          cargo
          rustfmt
          clippy
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
        ];

        RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
      };
    });
  };
}
