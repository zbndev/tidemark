{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.tidemark;
in
{
  options.services.tidemark = {
    enable = lib.mkEnableOption "the Tidemark user daemon";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.tidemark;
      defaultText = lib.literalExpression "inputs.tidemark.packages.\${pkgs.system}.tidemark";
      description = "The Tidemark package to run.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    services.dbus.packages = [ cfg.package ];
    systemd.user.services.tidemarkd = {
      description = "Tidemark quota daemon";
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      wantedBy = [ ];
      serviceConfig = {
        Type = "dbus";
        BusName = "io.github.zbndev.Tidemark.Daemon";
        ExecStart = "${cfg.package}/bin/tidemarkd";
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
      };
    };
  };
}
