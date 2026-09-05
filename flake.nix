{
  description = "sqlx-aip — rewrites an AIP List request into Postgres SQL fragments for sqlx";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # tests/postgres.rs needs a real database, so the shell ships one.
        #
        # A throwaway cluster on a non-default port, so it cannot collide with a
        # Postgres the developer already runs, bound to loopback so nothing here
        # is reachable off the machine. `trust` auth is safe on those terms and
        # nowhere else.
        port = "55432";
        database = "sqlx_aip_test";

        # Real executables rather than shell functions, because `nix develop
        # --command ...` execs a child process that would not inherit a function
        # defined in `shellHook`. CI runs in exactly that shape.
        pg-start = pkgs.writeShellScriptBin "pg-start" ''
          set -euo pipefail
          export PATH="${pkgs.postgresql}/bin:$PATH"
          if [[ ! -s "$PGDATA/PG_VERSION" ]]; then
            initdb -U postgres --auth=trust >/dev/null
          fi
          pg_ctl -o "-p ${port} -c listen_addresses=127.0.0.1" -l "$PGDATA/log" start
          createdb -h 127.0.0.1 -p ${port} -U postgres ${database} 2>/dev/null || true
        '';
        pg-stop = pkgs.writeShellScriptBin "pg-stop" ''
          set -euo pipefail
          export PATH="${pkgs.postgresql}/bin:$PATH"
          pg_ctl stop
        '';
      in
      {
        # No `packages.default`. This is a library crate with no binary, and
        # `buildRustPackage` would want a committed Cargo.lock -- which a library
        # deliberately does not have. The dev shell is the whole point here.
        devShells.default = pkgs.mkShell {
          inherit (manifest) name;

          packages = with pkgs; [
            rust-toolchain
            pkg-config
            # The server, not just `psql`: the helpers above spin a cluster out
            # of it, and `psql` is for poking at what tests/postgres.rs left.
            postgresql
            pg-start
            pg-stop
          ];

          # Set here rather than in `env`, because `${toString ./.}` resolves to
          # the read-only copy of the source in the Nix store rather than to the
          # checkout being worked in. `.pgdata` is gitignored.
          #
          # Both defer to an existing value. CI runs this shell with
          # `nix develop --command ...` and points DATABASE_URL at a service
          # container; overwriting it here would send the tests at a cluster
          # that does not exist there.
          shellHook = ''
            export PGDATA="''${PGDATA:-$PWD/.pgdata}"
            export DATABASE_URL="''${DATABASE_URL:-postgres://postgres@127.0.0.1:${port}/${database}}"

            # Only greet a human. CI drives this shell with
            # `nix develop --command ...`, where a banner is just noise in front
            # of the output someone is actually reading.
            if [[ $- == *i* ]]; then
              echo "${manifest.name} ${manifest.version} — $(cargo --version)"
              echo "  pg-start && cargo test      # incl. end-to-end tests/postgres.rs"
              echo "  cargo test                  # unit tests only; the rest skip"
              echo "  cargo clippy --all-targets"
              echo "  pg-stop"
            fi
          '';
        };
      }
    );
}
