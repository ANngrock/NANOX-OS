{
  description = "NANOX-OS M0 bench: pinned Clang/LLD 18, QEMU, OVMF, mtools, Python";

  # nixpkgs is pinned by commit.  The revision is the head of branch
  # nixos-24.11 as reported by
  #   git ls-remote https://github.com/NixOS/nixpkgs refs/heads/nixos-24.11
  # on 2026-09-24.  flake.lock (with the narHash of this revision) has not been
  # generated yet because Nix was not available in the environment that wrote
  # this file: run `nix flake lock` once and commit flake.lock.
  #
  # STATUS: written but never evaluated.  toolchain.lock marks the matching
  # "nix" profile as unverified until `make doctor && make && make test &&
  # make repro-check` has passed inside `nix develop`.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/50ab793786d9de88ee30ec4e4c24fb4236fc2674";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      llvm = pkgs.llvmPackages_18;
    in
    {
      devShells.${system}.default = pkgs.mkShellNoCC {
        packages = [
          llvm.clang # wrapped: host unit tests (HOST_CC)
          llvm.lld # ld.lld (kernel ELF) and lld-link (UEFI PE/COFF)
          pkgs.qemu
          pkgs.OVMF.fd
          pkgs.mtools
          pkgs.python3
          pkgs.gnumake
          pkgs.gdb
          pkgs.coreutils
          pkgs.git
        ];

        # Freestanding loader/kernel builds use the unwrapped compiler so that
        # the Nix cc-wrapper does not inject host hardening or libc flags.
        CLANG = "${llvm.clang-unwrapped}/bin/clang";
        HOST_CC = "clang";

        # Firmware used by tools/bench/qemu.py instead of the Ubuntu paths.
        NANOX_OVMF_CODE = pkgs.OVMF.firmware;
        NANOX_OVMF_VARS = pkgs.OVMF.variables;
        NANOX_TOOLCHAIN_PROFILE = "nix";
      };
    };
}
