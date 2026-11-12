{
  gitUrl,
  gitRev,
  gitRef ? "HEAD",
}:
{ lib, pkgs, ... }:
let
  src = builtins.fetchGit {
    url = gitUrl;
    rev = gitRev;
    ref = gitRef;
  };
  runtimeTools = [ pkgs.git pkgs.openssh pkgs.fuse3 ];
  package = pkgs.rustPlatform.buildRustPackage {
    pname = "grayskillfs";
    version = "0.1.0-${builtins.substring 0 7 gitRev}";
    inherit src;
    cargoLock.lockFile = src + "/Cargo.lock";

    nativeBuildInputs = [ pkgs.gnumake pkgs.makeWrapper ];
    nativeCheckInputs = [ pkgs.git ];
    doCheck = true;

    buildPhase = ''
      runHook preBuild
      make all
      runHook postBuild
    '';
    checkPhase = ''
      runHook preCheck
      make test
      runHook postCheck
    '';
    installPhase = ''
      runHook preInstall
      install -Dm755 target/release/grayskillfs "$out/bin/grayskillfs"
      mkdir -p "$out/lib/systemd/system"
      substitute ${./grayskillfs.service} "$out/lib/systemd/system/grayskillfs.service" \
        --replace-fail /usr/local/bin/grayskillfs "$out/bin/grayskillfs" \
        --replace-fail /usr/bin/install ${pkgs.coreutils}/bin/install \
        --replace-fail /usr/bin/fusermount3 ${pkgs.fuse3}/bin/fusermount3
      wrapProgram "$out/bin/grayskillfs" \
        --prefix PATH : ${lib.makeBinPath runtimeTools}
      runHook postInstall
    '';
  };
in
{
  boot.kernelModules = [ "fuse" ];
  environment.systemPackages = [ package ];
  environment.etc."grayskillfs.conf".source = ./grayskillfs.conf;

  users.groups.grayskillfs-git = { };
  users.users.grayskillfs-git = {
    isSystemUser = true;
    group = "grayskillfs-git";
  };

  systemd.tmpfiles.rules = [
    "d /srv/grayskillfs 0755 root root -"
    "d /srv/grayskillfs/base 0755 root root -"
    "d /srv/grayskillfs/data 0755 root root -"
  ];
  systemd.packages = [ package ];
  systemd.services.grayskillfs = {
    wantedBy = [ "multi-user.target" ];
    overrideStrategy = "asDropin";
    path = runtimeTools;
  };
}
