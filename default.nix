{
  lib,
  rustPlatform,
  pkg-config,
  wayland,
  libxkbcommon,
  libGL,
  libglvnd,
  libX11,
  libXcursor,
  libXrandr,
  libXi,
  libinput,
  seatd,
  systemdMinimal,
  libgbm,
  mesa,
  libdisplay-info,
  lua,
  gitRev ? null,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "raven";
  version = if gitRev != null then lib.substring 0 8 gitRev else "dev";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [pkg-config];

  buildInputs = [
    wayland
    libxkbcommon
    libGL
    libglvnd
    libX11
    libXcursor
    libXrandr
    libXi
    libinput
    seatd
    systemdMinimal
    libgbm
    mesa
    libdisplay-info
    lua
  ];

  doCheck = false;

  meta = {
    description = "Wayland compositor written in Rust using smithay";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = "raven";
  };
})
