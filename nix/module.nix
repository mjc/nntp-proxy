{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.nntp-proxy;
  tomlFormat = pkgs.formats.toml {};
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  serviceEnvironment =
    cfg.environment
    // lib.optionalAttrs (cfg.credentialsFile != null) {
      NNTP_PROXY_CREDENTIALS_FILE = toString cfg.credentialsFile;
    };

  hasCache = lib.hasAttrByPath ["cache"] cfg.settings;
  storesArticleBodies = lib.attrByPath ["cache" "store_article_bodies"] false cfg.settings;
  hasDiskCache = lib.hasAttrByPath ["cache" "disk"] cfg.settings;
  configPath =
    if cfg.configFile != null
    then cfg.configFile
    else
      tomlFormat.generate "nntp-proxy.toml" (
        lib.foldl' lib.recursiveUpdate {} [
          {
            proxy.stats_file = "/var/lib/nntp-proxy/stats.json";
          }
          (lib.optionalAttrs (hasCache && !storesArticleBodies) {
            cache.availability_index_path = "/var/cache/nntp-proxy/availability.idx";
          })
          (lib.optionalAttrs hasDiskCache {
            cache.disk.path = "/var/cache/nntp-proxy/articles";
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

    systemd.tmpfiles.rules = lib.optional (cfg.configFile == null && hasDiskCache) "d /var/cache/nntp-proxy/articles 0750 - - - -";

    systemd.services.nntp-proxy = {
      description = "NNTP proxy service";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target"];
      environment = serviceEnvironment;

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs (
          [(lib.getExe cfg.package) "--ui" "headless" "--config" (toString configPath)]
          ++ cfg.extraArgs
        );
        DynamicUser = true;
        StateDirectory = "nntp-proxy";
        CacheDirectory = "nntp-proxy";
        WorkingDirectory = "/var/lib/nntp-proxy";
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
        ReadWritePaths = [
          "/var/lib/nntp-proxy"
          "/var/cache/nntp-proxy"
        ];
      };
    };
  };
}
