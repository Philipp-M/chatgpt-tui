{ nixpkgs ? import <nixpkgs> { } }:
nixpkgs.mkShell {
  name = "rust-env";
  nativeBuildInputs = with nixpkgs; [
    # rustc cargo

    # Example Build-time Additional Dependencies
    pkgconfig
  ];
  buildInputs = with nixpkgs; [
    # Example Run-time Additional Dependencies
    openssl
  ];

  # Set Environment Variables
  RUST_BACKTRACE = 1;
  LIBCLANG_PATH = "${nixpkgs.llvmPackages.libclang.lib}/lib";
}
