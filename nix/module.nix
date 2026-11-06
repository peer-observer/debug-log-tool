# NixOS module for debug-log-tool: timed `merge` and `templates` jobs.
#
#   services.debug-log-tool = {
#     merge.peerobserver = {
#       inputDir = "/var/lib/peer-observer/archiver";
#       outputDir = "/data/merged";
#       supplementaryGroups = [ "peer-observer" ];   # to read inputDir
#       postMerge.prune.script = ''
#         # $MERGED_DAYS lists the .zst written this run, one per line.
#         while read -r merged; do
#           date=$(basename "$merged" .zst); date=''${date#debug.log-}
#           rm -v "$INPUT_DIR"/debug.log-"$date"-*.gz
#         done < "$MERGED_DAYS"
#       '';
#     };
#
#     templates.catalogue = {
#       inputs = [ "/data/merged" ];
#       statePath = "/var/lib/debug-log-tool/catalogue.jsonl";  # set => accumulate
#       reportPath = "/data/reports/catalogue.json";
#     };
#   };
#
# Each merge instance is a timer that starts a target, which runs the merge
# and then every `postMerge` step in turn. A merge that fails — or whose
# `--check` finds a mismatch — publishes nothing and its post-merge steps do
# not run.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.debug-log-tool;

  defaultUser = "debug-log-tool";

  enabledMerge = lib.filterAttrs (_: m: m.enable) cfg.merge;
  enabledTemplates = lib.filterAttrs (_: t: t.enable) cfg.templates;
  allInstances = lib.attrValues enabledMerge ++ lib.attrValues enabledTemplates;

  # Neither subcommand touches the network, and both only need the paths they
  # are pointed at.
  hardening = {
    NoNewPrivileges = true;
    PrivateTmp = true;
    PrivateDevices = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    RestrictAddressFamilies = [ "AF_UNIX" ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallArchitectures = "native";
    SystemCallFilter = [
      "@system-service"
      "~@privileged"
    ];
    CapabilityBoundingSet = "";
    IPAddressDeny = "any";
    UMask = "0027";
  };

  # Options every instance shares: who runs it, when, and how much of the
  # machine it may take.
  commonOptions = kind: {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to run this ${kind} instance.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = cfg.package;
      defaultText = lib.literalExpression "config.services.debug-log-tool.package";
      description = "The debug-log-tool package to run.";
    };

    onCalendar = lib.mkOption {
      type = lib.types.str;
      default = "daily";
      example = "*-*-* 04:30:00";
      description = "`OnCalendar` of the timer that starts this instance.";
    };

    persistent = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run as soon as possible if the machine was off when the timer should have fired.";
    };

    randomizedDelaySec = lib.mkOption {
      type = lib.types.str;
      default = "0";
      example = "30min";
      description = "Spread the start over this long, so several instances don't begin at once.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = defaultUser;
      description = "User the job runs as. Needs to read the logs and write the output.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = defaultUser;
      description = "Group the job runs as.";
    };

    supplementaryGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "peer-observer" ];
      description = "Extra groups, for reading logs another service owns.";
    };

    memoryMax = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "4G";
      example = "16G";
      description = ''
        `MemoryMax` for the job, or null for no limit. Compression is the
        hungry part: a 1 GB day peaks around 1 GB of memory at `jobs = 8`
        and 2.9 GB at `jobs = 24`, and `days` multiplies it.
      '';
    };

    nice = lib.mkOption {
      type = lib.types.int;
      default = 10;
      description = "Scheduling priority, so an archival job doesn't crowd out a node on the same machine.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "--level"
        "15"
      ];
      description = "Extra arguments appended to the command.";
    };
  };

  postMergeOptions =
    { name, ... }:
    {
      options = {
        script = lib.mkOption {
          type = lib.types.lines;
          description = ''
            Shell run after a successful merge. `$MERGED_DAYS` is a file
            listing the `.zst` written this run, one path per line;
            `$INPUT_DIR` and `$OUTPUT_DIR` are the instance's directories.
            The step is skipped when nothing was merged.
          '';
        };

        path = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
          example = lib.literalExpression "[ pkgs.rclone ]";
          description = "Packages on `PATH` for the script.";
        };

        readWritePaths = lib.mkOption {
          type = lib.types.listOf lib.types.path;
          default = [ ];
          description = "Paths the step may write to, on top of the instance's input and output directories.";
        };
      };
    };

  mergeOptions =
    { name, ... }:
    {
      options = commonOptions "merge" // {
        inputDir = lib.mkOption {
          type = lib.types.path;
          example = "/var/lib/peer-observer/archiver";
          description = "Directory holding the per-node `debug.log-<date>-<node>.gz` files.";
        };

        outputDir = lib.mkOption {
          type = lib.types.path;
          example = "/data/merged";
          description = "Directory the per-day `debug.log-<date>.zst` files are written to.";
        };

        level = lib.mkOption {
          type = lib.types.ints.between 1 22;
          default = 19;
          description = "zstd compression level (`--level`).";
        };

        jobs = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 8;
          description = ''
            Compression worker threads (`--jobs`); 0 uses every core. Almost
            all of a merge's wall time is compression, and this is what buys
            it back — but see `memoryMax`.
          '';
        };

        days = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 1;
          description = ''
            Days merged side by side (`--days`); 0 uses every core. Useful
            when catching up on a backlog, and multiplies memory use.
          '';
        };

        check = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''
            Verify the merge by re-reading and splitting it back
            (`--check`). A mismatch leaves the log unpublished and fails the
            unit, so the post-merge steps don't run.
          '';
        };

        skipCurrentDay = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''
            Pass today's date to `--before`, leaving the day still being
            written alone. Turn this off only if nothing is appending to the
            logs any more.
          '';
        };

        postMerge = lib.mkOption {
          type = lib.types.attrsOf (lib.types.submodule postMergeOptions);
          default = { };
          description = "Steps to run after a successful merge, each its own unit, in attribute-name order.";
        };
      };
    };

  templatesOptions =
    { name, ... }:
    {
      options = commonOptions "templates" // {
        inputs = lib.mkOption {
          type = lib.types.listOf lib.types.path;
          example = [ "/data/merged" ];
          description = ''
            Logs to read: each entry is either a log file or a directory,
            in which case every `debug.log-*` file directly inside it is
            read. Compression is detected per file.
          '';
        };

        statePath = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/var/lib/debug-log-tool/catalogue.jsonl";
          description = ''
            Set this to accumulate one catalogue across runs: each log is
            folded into the state once, and `reportPath` is rendered from
            it. Leave it null to report on each log on its own, into
            `reportDir`.
          '';
        };

        reportPath = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/data/reports/catalogue.json";
          description = "Where the accumulated report is written. Required when `statePath` is set.";
        };

        reportDir = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = "/data/reports";
          description = "Directory for one report per log. Required when `statePath` is null.";
        };

        format = lib.mkOption {
          type = lib.types.enum [
            "json"
            "text"
          ];
          default = "json";
          description = "Report format: NDJSON (`--json`) or the grouped text report.";
        };

        depth = lib.mkOption {
          type = lib.types.ints.positive;
          default = 4;
          description = "Drain tree depth (`--depth`). Ignored when loading an existing state, which carries its own.";
        };

        threshold = lib.mkOption {
          type = lib.types.float;
          default = 0.5;
          description = "Drain similarity threshold (`--threshold`).";
        };

        minCount = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 1;
          description = "Drop templates seen fewer times than this from the report (`--min-count`).";
        };

        top = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.positive;
          default = null;
          description = "Only report this many templates per category (`--top`).";
        };

        categories = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          example = [
            "net"
            "mempool"
          ];
          description = "Restrict the report to these categories (`--category`).";
        };
      };
    };

  # Shared by every unit: the sandbox, the resource limits and the identity.
  serviceConfigFor =
    instance:
    {
      extraReadWrite ? [ ],
      runtimeDir,
    }:
    hardening
    // {
      Type = "oneshot";
      User = instance.user;
      Group = instance.group;
      SupplementaryGroups = instance.supplementaryGroups;
      Nice = instance.nice;
      RuntimeDirectory = runtimeDir;
      RuntimeDirectoryPreserve = "yes";
      ReadWritePaths = extraReadWrite;
    }
    // lib.optionalAttrs (instance.memoryMax != null) { MemoryMax = instance.memoryMax; };

  timerFor = starts: instance: {
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = instance.onCalendar;
      Persistent = instance.persistent;
      RandomizedDelaySec = instance.randomizedDelaySec;
      Unit = starts;
    };
  };

  mergeScript =
    m:
    let
      args = [
        "--level"
        (toString m.level)
        "--jobs"
        (toString m.jobs)
        "--days"
        (toString m.days)
      ]
      ++ lib.optional m.check "--check"
      ++ m.extraArgs;
    in
    ''
      set -euo pipefail
      export INPUT_DIR=${lib.escapeShellArg m.inputDir}
      export OUTPUT_DIR=${lib.escapeShellArg m.outputDir}
      manifest="$RUNTIME_DIRECTORY/merged-days"
      : > "$manifest"

      before=""
      ${lib.optionalString m.skipCurrentDay "before=$(date -u +%Y%m%d)"}

      # Which days this run may touch, read off the filenames the same way
      # the tool reads them, so the manifest matches what it merged.
      days=$(
        find "$INPUT_DIR" -maxdepth 1 -type f -name 'debug.log-*-*.gz' -printf '%f\n' \
          | sed -n 's/^debug\.log-\([^-]*\)-.*\.gz$/\1/p' \
          | sort -u \
          | if [ -n "$before" ]; then awk -v b="$before" '$0 < b'; else cat; fi
      )
      if [ -z "$days" ]; then
        echo "nothing to merge in $INPUT_DIR"
        exit 0
      fi

      ${lib.getExe m.package} merge \
        ${lib.escapeShellArgs args} \
        ''${before:+--before "$before"} \
        --output-dir "$OUTPUT_DIR" \
        "$INPUT_DIR"

      for day in $days; do
        printf '%s\n' "$OUTPUT_DIR/debug.log-$day.zst" >> "$manifest"
      done
      echo "merged $(wc -l < "$manifest") day(s)"
    '';

  templatesScript =
    t:
    let
      reportArgs = [
        "--min-count"
        (toString t.minCount)
      ]
      ++ lib.optionals (t.top != null) [
        "--top"
        (toString t.top)
      ]
      ++ lib.concatMap (c: [
        "--category"
        c
      ]) t.categories
      ++ lib.optional (t.format == "json") "--json";
      newStateArgs = [
        "--depth"
        (toString t.depth)
        "--threshold"
        (toString t.threshold)
      ];
      tool = "${lib.getExe t.package} templates";
    in
    ''
      set -euo pipefail

      list_inputs() {
        for entry in ${lib.escapeShellArgs t.inputs}; do
          if [ -d "$entry" ]; then
            find "$entry" -maxdepth 1 -type f -name 'debug.log-*' | sort
          elif [ -f "$entry" ]; then
            printf '%s\n' "$entry"
          fi
        done
      }

      ${
        if t.statePath != null then
          ''
            state=${lib.escapeShellArg t.statePath}
            seen="$state.seen"
            touch "$seen"
            list_inputs | while read -r log; do
              # Each log is folded in once, so counts don't double on re-runs.
              if grep -qxF "$log" "$seen"; then continue; fi
              if [ -e "$state" ]; then
                ${tool} --load-state "$state" --save-state "$state" "$log" > /dev/null
              else
                ${tool} ${lib.escapeShellArgs newStateArgs} --save-state "$state" "$log" > /dev/null
              fi
              printf '%s\n' "$log" >> "$seen"
              echo "ingested $log"
            done

            if [ -e "$state" ]; then
              report=${lib.escapeShellArg t.reportPath}
              ${tool} --load-state "$state" ${lib.escapeShellArgs reportArgs} > "$report.tmp"
              mv "$report.tmp" "$report"
              echo "wrote $report"
            fi
          ''
        else
          ''
            list_inputs | while read -r log; do
              report=${lib.escapeShellArg t.reportDir}/$(basename "$log").${
                if t.format == "json" then "json" else "txt"
              }
              ${tool} ${lib.escapeShellArgs reportArgs} "$log" > "$report.tmp"
              mv "$report.tmp" "$report"
              echo "wrote $report"
            done
          ''
      }
    '';

  mergeUnitName = name: "debug-log-tool-merge-${name}";
  templatesUnitName = name: "debug-log-tool-templates-${name}";

  # A merge instance: timer -> target -> merge service -> post-merge services.
  # The target is held up by its services and stops once they are done, so a
  # post-merge step can't be cut short. See
  # https://serverfault.com/a/1128671/951735
  mergeUnits =
    name: m:
    let
      unit = mergeUnitName name;
      runtimeDir = unit;
      postMergeNames = lib.attrNames m.postMerge;
      # Steps run in attribute-name order, each after the one before it.
      previous =
        i:
        if i == 0 then
          [ "${unit}.service" ]
        else
          [ "${unit}-${lib.elemAt postMergeNames (i - 1)}.service" ];
    in
    {
      timers.${unit} = timerFor "${unit}.target" m;

      targets.${unit} = {
        description = "debug-log-tool merge ${name}";
        unitConfig.StopWhenUnneeded = "true";
      };

      services = {
        ${unit} = {
          description = "debug-log-tool merge ${name}";
          after = [ "${unit}.target" ];
          wantedBy = [ "${unit}.target" ];
          unitConfig.Upholds = "${unit}.target";
          path = [
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnused
            pkgs.gawk
          ];
          script = mergeScript m;
          serviceConfig = serviceConfigFor m {
            inherit runtimeDir;
            extraReadWrite = [ m.outputDir ];
          };
        };
      }
      // lib.listToAttrs (
        lib.imap0 (
          i: stepName:
          let
            step = m.postMerge.${stepName};
          in
          lib.nameValuePair "${unit}-${stepName}" {
            description = "debug-log-tool merge ${name}: ${stepName}";
            after = [ "${unit}.target" ] ++ previous i;
            wantedBy = [ "${unit}.target" ];
            unitConfig.Upholds = "${unit}.target";
            path = step.path ++ [ pkgs.coreutils ];
            environment = {
              MERGED_DAYS = "/run/${runtimeDir}/merged-days";
              INPUT_DIR = m.inputDir;
              OUTPUT_DIR = m.outputDir;
            };
            script = step.script;
            serviceConfig =
              serviceConfigFor m {
                inherit runtimeDir;
                extraReadWrite = [
                  m.inputDir
                  m.outputDir
                ]
                ++ step.readWritePaths;
              }
              # Nothing merged means nothing to do, and systemd skips the unit.
              // {
                ExecCondition = "${pkgs.coreutils}/bin/test -s /run/${runtimeDir}/merged-days";
              };
          }
        ) postMergeNames
      );
    };

  templatesUnits =
    name: t:
    let
      unit = templatesUnitName name;
    in
    {
      timers.${unit} = timerFor "${unit}.service" t;
      targets = { };
      services.${unit} = {
        description = "debug-log-tool templates ${name}";
        path = [
          pkgs.coreutils
          pkgs.findutils
          pkgs.gnugrep
        ];
        script = templatesScript t;
        serviceConfig = serviceConfigFor t {
          runtimeDir = unit;
          extraReadWrite =
            lib.optional (t.statePath != null) (builtins.dirOf t.statePath)
            ++ lib.optional (t.reportPath != null) (builtins.dirOf t.reportPath)
            ++ lib.optional (t.reportDir != null) t.reportDir;
        };
      };
    };

  # The timer starts the target, not the service.
  templatesTimerUnit = unit: {
    OnCalendar = null;
  };

  allMergeUnits = lib.mapAttrsToList mergeUnits enabledMerge;
  allTemplatesUnits = lib.mapAttrsToList templatesUnits enabledTemplates;
  collect =
    attr: lib.foldl' (acc: u: acc // (u.${attr} or { })) { } (allMergeUnits ++ allTemplatesUnits);

  # Directories the jobs write to, created before they run.
  tmpfilesFor =
    instance: paths:
    map (p: "d ${p} 0750 ${instance.user} ${instance.group} -") (lib.filter (p: p != null) paths);
in
{
  options.services.debug-log-tool = {
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "the flake's default package";
      description = "The debug-log-tool package used by instances that don't override it.";
    };

    merge = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule mergeOptions);
      default = { };
      description = "Timed `merge` jobs, one per attribute name.";
    };

    templates = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule templatesOptions);
      default = { };
      description = "Timed `templates` jobs, one per attribute name.";
    };
  };

  config = lib.mkIf (enabledMerge != { } || enabledTemplates != { }) {
    assertions =
      lib.mapAttrsToList (name: t: {
        assertion = (t.statePath != null) -> (t.reportPath != null && t.reportDir == null);
        message = "services.debug-log-tool.templates.${name}: with statePath set (one accumulating catalogue), set reportPath and leave reportDir unset.";
      }) enabledTemplates
      ++ lib.mapAttrsToList (name: t: {
        assertion = (t.statePath == null) -> (t.reportDir != null && t.reportPath == null);
        message = "services.debug-log-tool.templates.${name}: without statePath (a report per log), set reportDir and leave reportPath unset.";
      }) enabledTemplates
      ++ lib.mapAttrsToList (name: t: {
        assertion = t.inputs != [ ];
        message = "services.debug-log-tool.templates.${name}: inputs is empty, so there is nothing to read.";
      }) enabledTemplates;

    users.users = lib.optionalAttrs (lib.any (i: i.user == defaultUser) allInstances) {
      ${defaultUser} = {
        isSystemUser = true;
        group = defaultUser;
        description = "debug-log-tool jobs";
      };
    };

    users.groups = lib.optionalAttrs (lib.any (i: i.group == defaultUser) allInstances) {
      ${defaultUser} = { };
    };

    systemd.tmpfiles.rules = lib.unique (
      lib.flatten (
        lib.mapAttrsToList (_: m: tmpfilesFor m [ m.outputDir ]) enabledMerge
        ++ lib.mapAttrsToList (
          _: t:
          tmpfilesFor t [
            (if t.statePath != null then builtins.dirOf t.statePath else null)
            (if t.reportPath != null then builtins.dirOf t.reportPath else null)
            t.reportDir
          ]
        ) enabledTemplates
      )
    );

    systemd.timers = collect "timers";
    systemd.targets = collect "targets";
    systemd.services = collect "services";
  };
}
