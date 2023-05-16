{
  description = "Simple ChatGPT TUI using the API";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    # nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.flake-utils.follows = "flake-utils";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, flake-utils, nixpkgs, rust-overlay }:
    let
      inherit (builtins) substring;
      inherit (nixpkgs) lib;

      mtime = self.lastModifiedDate;
      date = "${substring 0 4 mtime}-${substring 4 2 mtime}-${substring 6 2 mtime}";

      mkChatGPTTui = { rustPlatform, xorg, ... }:
        rustPlatform.buildRustPackage {
          pname = "chatgpt-tui";
          version = "unstable-${date}";
          src = self;
          cargoLock.lockFile = self + "/Cargo.lock";
        };
    in
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rustPkgs = rust-overlay.packages.${system};
        in
        {
          packages = rec {
            default = chatgpt-tui;
            chatgpt-tui = pkgs.callPackage mkChatGPTTui { };
          };

          devShells.default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; let
              vers = lib.splitVersion rustc.version;
              rustVersion = "${lib.elemAt vers 0}_${lib.elemAt vers 1}_${lib.elemAt vers 2}";
            in
            [
              # Follows nixpkgs's version of rustc.
              rustPkgs."rust_${rustVersion}"
              nixpkgs-fmt
              # cargo-flamegraph
              xorg.libxcb
            ];

            RUST_BACKTRACE = "short";
            NIXPKGS = nixpkgs;
          };
        })
    // {
      overlays = rec {
        default = chatgpt-tui;
        chatgpt-tui = final: prev: {
          chatgpt-tui = final.callPackage mkChatGPTTui { };
        };
      };
    };
}

