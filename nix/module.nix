{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.nntp-proxy;
  tomlFormat = pkgs.formats.toml {};
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  managedStateDir = "/var/lib/nntp-proxy";
  managedCacheDir = "/var/cache/nntp-proxy";
  managedStatsPath = "${managedStateDir}/stats.json";
  managedAvailabilityPath = "${managedCacheDir}/availability.idx";
  managedDiskCachePath = "${managedCacheDir}/disk-cache";
  managedExternalConfigPath = "${managedStateDir}/external-config.toml";
  asAbsoluteString = value: let
    rendered = toString value;
  in
    if lib.hasPrefix "/" rendered
    then rendered
    else null;
  serviceEnvironment =
    cfg.environment
    // lib.optionalAttrs (cfg.credentialsFile != null) {
      NNTP_PROXY_CREDENTIALS_FILE = toString cfg.credentialsFile;
    };

  hasCache = lib.hasAttrByPath ["cache"] cfg.settings;
  storesArticleBodies = lib.attrByPath ["cache" "store_article_bodies"] false cfg.settings;
  hasDiskCache = lib.hasAttrByPath ["cache" "disk"] cfg.settings;
  generatedStatsPath = lib.attrByPath ["proxy" "stats_file"] managedStatsPath cfg.settings;
  generatedAvailabilityPath =
    if hasCache && !storesArticleBodies
    then lib.attrByPath ["cache" "availability_index_path"] managedAvailabilityPath cfg.settings
    else null;
  generatedDiskCachePath =
    if hasDiskCache
    then lib.attrByPath ["cache" "disk" "path"] managedDiskCachePath cfg.settings
    else null;
  customGeneratedStatsPath =
    if cfg.configFile == null && asAbsoluteString generatedStatsPath != null && asAbsoluteString generatedStatsPath != managedStatsPath
    then asAbsoluteString generatedStatsPath
    else null;
  customGeneratedAvailabilityPath =
    if cfg.configFile == null && generatedAvailabilityPath != null && asAbsoluteString generatedAvailabilityPath != null && asAbsoluteString generatedAvailabilityPath != managedAvailabilityPath
    then asAbsoluteString generatedAvailabilityPath
    else null;
  customGeneratedDiskCachePath =
    if cfg.configFile == null && generatedDiskCachePath != null && asAbsoluteString generatedDiskCachePath != null && asAbsoluteString generatedDiskCachePath != managedDiskCachePath
    then asAbsoluteString generatedDiskCachePath
    else null;
  configPath =
    if cfg.configFile != null
    then managedExternalConfigPath
    else
      tomlFormat.generate "nntp-proxy.toml" (
        lib.foldl' lib.recursiveUpdate {} [
          {
            proxy.stats_file = managedStatsPath;
          }
          (lib.optionalAttrs (hasCache && !storesArticleBodies) {
            cache.availability_index_path = managedAvailabilityPath;
          })
          (lib.optionalAttrs hasDiskCache {
            cache.disk.path = managedDiskCachePath;
          })
          cfg.settings
        ]
      );
  listenPort = lib.attrByPath ["proxy" "port"] cfg.listenPort cfg.settings;
in {
  disabledModules = ["services/networking/nntp-proxy.nix"];

  options.services.nntp-proxy = {
    enable = lib.mkEnableOption "the nntp-proxy service";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      defaultText = lib.literalExpression "inputs.nntp-proxy.packages.\${pkgs.system}.default";
      description = "Package providing the `nntp-proxy` executable.";
    };

    configFile = lib.mkOption {
      type = with lib.types; nullOr (either path str);
      default = null;
      example = "/run/secrets/nntp-proxy.toml";
      description = ''
        Existing TOML config file to pass to `nntp-proxy --config`.
        When unset, the module renders one from `services.nntp-proxy.settings`.
      '';
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = {};
      example = lib.literalExpression ''
        {
          proxy.port = 8119;
          routing.mode = "hybrid";
          servers = [
            {
              host = "news.example.com";
              port = 563;
              name = "Primary";
              use_tls = true;
              tls_verify_cert = true;
              max_connections = 20;
            }
          ];
        }
      '';
      description = ''
        Declarative `config.toml` contents. The module injects writable defaults for
        `proxy.stats_file`, availability persistence, and disk-cache storage so the
        generated config works under systemd without pointing into the Nix store.
      '';
    };

    extraArgs = lib.mkOption {
      type = with lib.types; listOf str;
      default = [];
      example = ["--routing-mode" "stateful"];
      description = "Additional arguments appended to the `nntp-proxy` command line.";
    };

    environment = lib.mkOption {
      type = with lib.types; attrsOf str;
      default = {};
      example = {
        RUST_LOG = "info";
      };
      description = "Environment variables exported to the systemd service.";
    };

    credentialsFile = lib.mkOption {
      type = with lib.types; nullOr (either path str);
      default = null;
      example = "/var/lib/nntp-proxy/credentials.toml";
      description = ''
        Optional credentials-only TOML overlay. When set, the service loads the
        main config from `settings` or `configFile`, then merges backend and
        client-auth passwords from this runtime file via
        `NNTP_PROXY_CREDENTIALS_FILE`.
      '';
    };

    openFirewall = lib.mkEnableOption "opening the NNTP listener port in the firewall";

    listenPort = lib.mkOption {
      type = lib.types.port;
      default = 8119;
      description = ''
        Firewall port fallback when `openFirewall` is enabled and the service port
        cannot be derived from `services.nntp-proxy.settings.proxy.port`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.configFile == null || cfg.settings == {};
        message = "services.nntp-proxy.configFile and services.nntp-proxy.settings are mutually exclusive.";
      }
    ];

    networking.firewall.allowedTCPPorts = lib.optional cfg.openFirewall listenPort;

    systemd.services.nntp-proxy = {
      description = "NNTP proxy service";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target"];
      environment = serviceEnvironment;
      preStart = lib.optionalString hasDiskCache ''
        ${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg managedDiskCachePath}
      '';

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs (
          [(lib.getExe cfg.package) "--ui" "headless" "--config" (toString configPath)]
          ++ cfg.extraArgs
        );
        DynamicUser = true;
        StateDirectory = "nntp-proxy";
        CacheDirectory = "nntp-proxy";
        WorkingDirectory = managedStateDir;
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectControlGroups = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictSUIDSGID = true;
        BindReadOnlyPaths = lib.optional (cfg.configFile != null) "${toString cfg.configFile}:${managedExternalConfigPath}";
        ReadWritePaths = [
          managedStateDir
          managedCacheDir
        ];
      };
    };

    systemd.tmpfiles.rules =
      (lib.optional (customGeneratedStatsPath != null) "L+ ${customGeneratedStatsPath} - - - - ${managedStatsPath}")
      ++ (lib.optional (customGeneratedAvailabilityPath != null) "L+ ${customGeneratedAvailabilityPath} - - - - ${managedAvailabilityPath}")
      ++ (lib.optional (customGeneratedDiskCachePath != null) "L+ ${customGeneratedDiskCachePath} - - - - ${managedDiskCachePath}");
  };
}
