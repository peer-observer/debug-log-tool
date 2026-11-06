{ rustPlatform, lib }:

rustPlatform.buildRustPackage {
  pname = "debug-log-tool";
  version = "0.1.0";
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "Tools for working with Bitcoin Core debug.log files";
    homepage = "https://github.com/peer-observer/debug-log-tool";
    license = lib.licenses.mit;
    mainProgram = "debug-log-tool";
  };
}
