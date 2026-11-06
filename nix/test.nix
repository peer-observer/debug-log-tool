# NixOS VM test of the debug-log-tool module: a merge instance with a
# post-merge step, and both shapes of templates instance.
{ self, pkgs }:

pkgs.testers.runNixOSTest {
  name = "debug-log-tool-module";

  nodes.machine =
    { config, ... }:
    {
      imports = [ self.nixosModules.default ];

      environment.systemPackages = [
        pkgs.gzip
        pkgs.zstd
        config.services.debug-log-tool.package
      ];

      services.debug-log-tool = {
        merge.logs = {
          inputDir = "/srv/logs";
          outputDir = "/srv/merged";
          # Fast, so the test isn't waiting on level 19.
          level = 3;
          jobs = 2;
          postMerge.record = {
            script = ''
              # Prove the step saw what was merged, and that it may write to
              # the input directory — where disposing of the originals goes.
              cp "$MERGED_DAYS" /srv/merged-days.txt
              wc -l < "$MERGED_DAYS" > /srv/merged-count.txt
              rm -f "$INPUT_DIR"/.probe
            '';
            readWritePaths = [ "/srv" ];
          };
        };

        templates.catalogue = {
          inputs = [ "/srv/merged" ];
          statePath = "/var/lib/debug-log-tool/catalogue.jsonl";
          reportPath = "/srv/reports/catalogue.json";
        };

        templates.perfile = {
          inputs = [ "/srv/merged" ];
          reportDir = "/srv/per-file";
          format = "text";
        };
      };

      systemd.tmpfiles.rules = [
        "d /srv 0755 root root -"
        "d /srv/logs 0750 debug-log-tool debug-log-tool -"
      ];
    };

  testScript = ''
    import datetime

    today = datetime.datetime.now(datetime.UTC).strftime("%Y%m%d")
    old = ["20200101", "20200102"]

    machine.wait_for_unit("multi-user.target")

    # Two complete days and the day still being written.
    for date in old + [today]:
        stamp = f"{date[0:4]}-{date[4:6]}-{date[6:8]}"
        for node in ["alice", "bob"]:
            machine.succeed(
                f"printf '%s\\n%s\\n' "
                f"'{stamp}T00:00:01 [net] received: ping (8 bytes) peer=1' "
                f"'{stamp}T00:00:02 [net] got inv: wtx abc have peer=2' "
                f"| gzip > /srv/logs/debug.log-{date}-{node}.gz"
            )
    machine.succeed("touch /srv/logs/.probe")
    machine.succeed("chown -R debug-log-tool:debug-log-tool /srv/logs")

    with subtest("the timer is installed but has not fired yet"):
        machine.succeed("systemctl is-active debug-log-tool-merge-logs.timer")
        machine.fail("test -e /srv/merged/debug.log-20200101.zst")

    with subtest("merging leaves the current day alone"):
        machine.succeed("systemctl start debug-log-tool-merge-logs.target")
        machine.wait_until_succeeds("test -f /srv/merged-count.txt", timeout=90)
        for date in old:
            machine.succeed(f"test -s /srv/merged/debug.log-{date}.zst")
        machine.fail(f"test -e /srv/merged/debug.log-{today}.zst")
        # The inputs are still there: disposing of them is the script's job.
        machine.succeed(f"test -f /srv/logs/debug.log-{today}-alice.gz")

    with subtest("the merged log round-trips and carries the node prefix"):
        machine.succeed(
            "zstd -dc /srv/merged/debug.log-20200101.zst | grep -q '^alice 2020-01-01T00:00:01 '"
        )
        machine.succeed(
            "test $(zstd -dc /srv/merged/debug.log-20200101.zst | wc -l) -eq 4"
        )

    with subtest("the post-merge step ran once, with the manifest"):
        assert machine.succeed("cat /srv/merged-count.txt").strip() == "2"
        manifest = machine.succeed("cat /srv/merged-days.txt")
        assert "/srv/merged/debug.log-20200101.zst" in manifest, manifest
        assert "/srv/merged/debug.log-20200102.zst" in manifest, manifest
        # It could write to the input directory.
        machine.fail("test -e /srv/logs/.probe")

    with subtest("nothing to merge is a clean no-op, and skips the step"):
        # Only the day still being written is left, so nothing is eligible.
        machine.succeed("rm /srv/logs/debug.log-20200101-*.gz /srv/logs/debug.log-20200102-*.gz")
        machine.succeed("rm /srv/merged-count.txt")
        machine.succeed("systemctl start debug-log-tool-merge-logs.target")
        machine.succeed("journalctl -u debug-log-tool-merge-logs.service | grep -q 'nothing to merge'")
        # ExecCondition saw an empty manifest, so the post-merge step did not run.
        machine.fail("test -e /srv/merged-count.txt")
        # And the tool itself says so rather than failing.
        machine.succeed(
            "debug-log-tool merge --before 20200101 -o /srv/none /srv/logs"
            " | grep -q 'nothing dated before'"
        )
        machine.fail("test -e /srv/none")

    with subtest("templates accumulates one catalogue"):
        machine.succeed("systemctl start debug-log-tool-templates-catalogue.service")
        machine.succeed("test -s /var/lib/debug-log-tool/catalogue.jsonl")
        machine.succeed("grep -q '\"category\":\"net\"' /srv/reports/catalogue.json")
        first = machine.succeed("cat /srv/reports/catalogue.json")
        # A second run must not count the same logs twice.
        machine.succeed("systemctl start debug-log-tool-templates-catalogue.service")
        assert machine.succeed("cat /srv/reports/catalogue.json") == first

    with subtest("templates reports per file"):
        machine.succeed("systemctl start debug-log-tool-templates-perfile.service")
        machine.succeed("test -s /srv/per-file/debug.log-20200101.zst.txt")
        machine.succeed("grep -q '=== net ===' /srv/per-file/debug.log-20200101.zst.txt")
  '';
}
