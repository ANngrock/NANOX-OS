{
  description = "NANOX M0: pinned Rust, UEFI firmware and QEMU test environment";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/ac62194c3917d5f474c1a844b6fd6da2db95077d";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/b88764907c1b2de6c0e206d8d9d9634d91c81334";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
      # The locked overlay supplies this exact Rust and both target libraries.
      rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      # QEMU 9.2 stores shutdown causes but loses isa-debug-exit's status in
      # replay. Preserve the status in a deliberately incompatible trace format.
      qemu = (pkgs.qemu.override {
        minimal = true;
        hostCpuTargets = [ "x86_64-softmmu" ];
        enableTools = true;
        enableBlobs = true;
      }).overrideAttrs (old: {
        patches = (old.patches or []) ++ [ ./tools/qemu/replay-exit-code.patch ];
        configureFlags = old.configureFlags ++ [ "--with-pkgversion=nanox-replay-exit-v1" ];
      });
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust qemu pkgs.OVMF pkgs.gptfdisk pkgs.dosfstools
          pkgs.mtools pkgs.git pkgs.gdb pkgs.python3 pkgs.coreutils pkgs.binutils ];
        NANOX_OVMF_CODE = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
        NANOX_OVMF_VARS = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";
        NANOX_QEMU_MACHINE = "pc-q35-9.2";
        NANOX_PINNED_RUST = "${rust}";
        SOURCE_DATE_EPOCH = "1790035200";
        TZ = "UTC";
        LC_ALL = "C";
        shellHook = ''
          export PATH="${rust}/bin:$PATH"
          echo "NANOX M0: Rust 1.90.0; use cargo xtask doctor"
        '';
      };
    };
}
