# Secondary-skimmer field node runbook (MAN-96)

How to run a real manta node for 30 days as a secondary skimmer behind an RBN Aggregator, next
to a trusted primary CW Skimmer/SkimSrv, and how to record the evidence that it worked. Every
command below is meant to be copied as written, after replacing the example values set at the
top of each block.

The decisions this procedure applies (thresholds, the watchdog, the comparison method) are
D-A…D-N in
[the MAN-96 decision record](../DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md).
This runbook says how; the decision record says why.

## 1. What this proves

MAN-96 has two scenarios. The run is done when both are recorded in a field report under
`docs/DECISIONS/` (template in section 10).

- **Scenario 1, 30 days unattended.** The node runs for 30 days as a secondary skimmer behind an
  admitted Aggregator, decoding and spotting, with no manual intervention beyond planned
  restarts. Evidence: the ledger written by `field-node.py sample` and the verdict of
  `field-node.py report --min-days 30` (exit 0 = PASS).
- **Scenario 2, compared against the primary.** After at least 7 days, the node's spots are
  compared with the primary skimmer's spots for the same window and the comparison is recorded.
  Evidence: the node's spot archive written by `field-node.py record-spots`, the RBN daily
  archives, and the output of `shadow-compare.py`.

The evidence files, all on the node:

| File | Written by | Holds |
|---|---|---|
| `/var/lib/manta-field/ledger.jsonl` | `manta-field-ledger.service` | one sample per minute (`/healthz`, `/metrics`), notes, watchdog recoveries |
| `/var/lib/manta-field/spots/spots-YYYY-MM-DD.jsonl` | `manta-field-spots.service` | every spot from the JSON Lines port, verbatim, one file per UTC day |

The tools are [`scripts/field-node.py`](../../scripts/field-node.py) and
[`scripts/shadow-compare.py`](../../scripts/shadow-compare.py). For what `/healthz` and each
`/metrics` family mean, see [node-health.md](node-health.md).

## 2. Before you start

- **Same antenna as the primary** (D-K): a splitter on the primary's feedline, or a second SDR on
  the same feedline. Differences in the comparison should come from the decoder, not the antenna.
  Any input type works; the example config uses SoapySDR/SDRplay because that is the only family
  with field evidence.
- **A Linux host with systemd** (D-J). Use an x86 mini-PC or a Raspberry Pi 5. Do not run this on
  a Pi 4 or claim Pi 4 results: the Pi 4 CPU budget is unproven and paused (D6 of
  [the 2026-09-06 broad review](../DECISIONS/2026-09-06-broad-review-decisions.md)).
- **NTP synchronized.** The comparison matches spots by time, so the clock matters:

  ```sh
  timedatectl status   # "System clock synchronized: yes"
  ```

- **The Aggregator operator's agreement**, and the Aggregator host's IP address. You need it for
  the firewall (section 3) and for Stage 2.
- **The primary's RBN spotter callsign** (the call its spots carry on RBN) and the frequency
  range the primary covers.
- **Build manta with the `soapy` feature.** Release binaries ship `hpsdr` but not `soapy`
  (`.github/workflows/release.yml`), so build from source. This needs `libsoapysdr-dev`, the
  SoapySDRPlay3 module and the SDRplay API service installed first. Clone the repo to
  `/opt/manta`; the companion units run the scripts from there.

  ```sh
  sudo git clone https://github.com/HagaleTechnologies/manta.git /opt/manta
  sudo chown -R "$USER" /opt/manta
  cd /opt/manta
  cargo build --release -p manta-cli --features soapy
  sudo install -m 0755 target/release/manta /usr/local/bin/manta
  manta --version   # e.g. manta 0.1.0 (git 1a2b3c4d5e6f; features: soapy) -- check soapy is listed
  git -C /opt/manta rev-parse HEAD   # record this SHA in the field report
  ```

  For an OpenHPSDR receiver, build with `--features hpsdr` instead or use a release binary; a
  KiwiSDR needs no feature. Once the node runs, `manta_build_info` on
  `/metrics` carries the same SHA in its `git_sha` label, and the ledger records it every minute.

## 3. Install

This kit is specific to the field run. For a general unattended node, use the
[service kit](../../packaging/README.md) (systemd, macOS launchd or Docker Compose); the
field node keeps its own static `manta` user and units below.

The kit lives in `docs/RUNBOOKS/field-node/`:

| File | Installs to |
|---|---|
| [manta-field.toml](field-node/manta-field.toml) | `/etc/manta/manta-field.toml` |
| [manta-field.service](field-node/manta-field.service) | `/etc/systemd/system/` |
| [manta-field-ledger.service](field-node/manta-field-ledger.service) | `/etc/systemd/system/` |
| [manta-field-spots.service](field-node/manta-field-spots.service) | `/etc/systemd/system/` |
| [manta-field-recover](field-node/manta-field-recover) | `/usr/local/sbin/`, mode 0755 |

User, config and check:

```sh
sudo useradd --system --groups plugdev manta
sudo install -d -m 0755 /etc/manta
sudo install -m 0644 /opt/manta/docs/RUNBOOKS/field-node/manta-field.toml /etc/manta/
sudoedit /etc/manta/manta-field.toml   # station_callsign, operator_*, [input]
manta config check --config /etc/manta/manta-field.toml
```

In the config, replace `N0CALL-1` with your call plus an RBN SSID (MAN-89; the Aggregator sees
`CALL-1-#`) and every `operator_*` value. Set `freq_hz` to the centre of the segment the primary
covers. `gain_db` on SDRplay is an attenuation scale, not a gain: sweep 10 to 20 as described in
[the 2026-09-09 gain record](../DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md).
Leave `freq_correction_ppm = 0.0` until Stage 1 measures it. Do not add an `[[rbn_uplink]]` table:
the Aggregator forwards this node's spots (D-L).

`manta config check` must print `valid`. It also prints a note that `bind_addr = "0.0.0.0"`
exposes the metrics endpoint on every interface; the firewall below handles that.

For a different receiver, swap the `[input]` table and keep the rest:

```toml
[input]
type = "kiwi"
host = "kiwi.example.org"
port = 8073
freq_hz = 7030000.0
```

```toml
[input]
type = "hpsdr"
host = "192.168.1.50"
port = 1024
freq_hz = 7030000.0
rate_hz = 192000.0
```

Units, recovery script and sudoers line:

```sh
cd /opt/manta/docs/RUNBOOKS/field-node
sudo install -m 0644 manta-field.service manta-field-ledger.service manta-field-spots.service /etc/systemd/system/
sudo install -m 0755 manta-field-recover /usr/local/sbin/manta-field-recover
echo 'manta ALL=(root) NOPASSWD: /usr/local/sbin/manta-field-recover' | sudo tee /etc/sudoers.d/manta-field
sudo chmod 0440 /etc/sudoers.d/manta-field
sudo visudo -c
sudo systemctl daemon-reload
```

If the SDR driver runs as its own service (the SDRplay API's `sdrplay.service`), order manta after
it with a drop-in:

```sh
sudo systemctl edit manta-field.service
# add:
# [Unit]
# After=sdrplay.service
# Wants=sdrplay.service
```

`manta-field-recover` restarts `sdrplay.service` if the host has it, then `manta-field.service`.
For a differently named driver service, edit the `SDR_SERVICE` default in
`/usr/local/sbin/manta-field-recover` (sudo does not pass the variable through).

Firewall (D-M). All three listeners share `bind_addr`, and a per-listener bind (MAN-132) has not
landed, so the firewall is what keeps JSON (7301) and metrics (7302) private while telnet (7300)
stays reachable. Only the Aggregator host may reach telnet; that is also what lets the ledger read
"a telnet client is connected" as "the Aggregator is connected" (D-B). Background:
[network-exposure.md](network-exposure.md).

With `ufw` (replace `203.0.113.10` with the Aggregator host's IP):

```sh
sudo ufw default deny incoming
sudo ufw allow OpenSSH
sudo ufw allow from 203.0.113.10 to any port 7300 proto tcp
sudo ufw enable
```

With `nft` (save as `/etc/nftables-fieldnode.nft` and include it from `/etc/nftables.conf`):

```
table inet fieldnode {
    chain input {
        type filter hook input priority 0; policy accept;
        iif "lo" accept
        tcp dport 7300 ip saddr 203.0.113.10 accept
        tcp dport { 7300, 7301, 7302 } drop
    }
}
```

From another machine on the LAN, `nc -vz <node> 7302` and `nc -vz <node> 7301` must fail.

## 4. Stage 0: bench (minutes)

With `manta-field.service` stopped (only one process can open the SDR):

```sh
sudo -u manta manta doctor --config /etc/manta/manta-field.toml --duration 120
```

Every check line must be `PASS` or `SKIP` (doctor exits 0 only when no check fails; a `WARN`
clock line still needs fixing before Stage 1, because spot times go to RBN), and the verdict must
be `DECODING`. See [setup-checks](setup-checks.md) for each check and its fix. A confirmed spot
alone is not proof of a real signal. If the
verdict is anything else, or the spots look wrong, follow
[live-hardware-field-testing](../../wiki/pages/live-hardware-field-testing.md) in order: the
gain-is-attenuation sweep first, then the raw channelizer power check against a known carrier
(WWV) before blaming the detector.

## 5. Stage 1: 24 h private soak and go/no-go

Aggregator forwarding is all or nothing: once the node is a secondary, its spots go to RBN unless
the Aggregator operator stops all forwarding (Aggregator manual §5.6). So the quality gate runs
**before** the node joins. Do not tell the Aggregator operator to add the node yet.

Start the three units, wait for the ledger's first scrape (one sample line in the ledger, about
60 s), then write the start note. A start note before the first sample leaves the spots emitted
before it uncounted, and `report` marks the missed-spot count indeterminate (NO-GO):

```sh
sudo systemctl enable --now manta-field.service manta-field-ledger.service manta-field-spots.service
until grep -q '"kind": *"sample"' /var/lib/manta-field/ledger.jsonl 2>/dev/null; do sleep 5; done
sudo -u manta python3 /opt/manta/scripts/field-node.py note \
  --ledger /var/lib/manta-field/ledger.jsonl --kind start --reason "stage 1"
curl -s http://127.0.0.1:7302/healthz   # "ok" and one line per check
tail -n 1 /var/lib/manta-field/ledger.jsonl
```

After 24 h, evaluate the ledger over the 24 h window. No Aggregator is connected in Stage 1, so
pass `--min-aggregator 0` to switch off the D-B criterion. Wait at least one scrape interval (60 s)
after `END` first: spots emitted between the last scrape and `END` only show on the next scrape,
and without it the missed-spot count below can come out low (MAN-291 makes `report` enforce this):

```sh
START=2026-11-01T12:00:00Z   # the stage 1 start note's time, UTC
END=2026-11-02T12:00:00Z     # START + 24 h
python3 /opt/manta/scripts/field-node.py report \
  --ledger /var/lib/manta-field/ledger.jsonl --spots-dir /var/lib/manta-field/spots \
  --from "$START" --to "$END" --min-days 1 --min-aggregator 0
```

Note the report's `Spots missed by the recorder: N` line (`## Daily`). Those are spots the node
emitted that `manta-field-spots` did not archive; any of them could be a false spot, so the
comparison below counts them as uncorroborated.

If manta restarted in the window, or no sample precedes the start note, that line reads
`indeterminate` (the spot counter is per process, and spots before the first sample are unseen);
pass `indeterminate` and Stage 1 is NO-GO.

```sh
MISSED=0   # N from "Spots missed by the recorder: N", or indeterminate
```

Download the RBN daily archive for every UTC date the window touches. A day's file appears after
that UTC day ends.

```sh
mkdir -p ~/rbn && cd ~/rbn
for d in 20261101 20261102; do
  curl -fsSO "https://data.reversebeacon.net/rbn_history/$d.zip"
done
```

Compare against the primary. `--passband-khz` is the range both the node and the primary cover;
repeat the flag for a split range.

```sh
PRIMARY=K1ABC   # the primary's RBN spotter callsign
python3 /opt/manta/scripts/shadow-compare.py \
  --node-spots /var/lib/manta-field/spots \
  --rbn ~/rbn/20261101.zip ~/rbn/20261102.zip \
  --primary "$PRIMARY" --passband-khz 7000-7060 \
  --start "$START" --end "$END" --missed-spots "$MISSED" \
  --json ~/stage1-shadow.json > ~/stage1-shadow.md
```

**Go/no-go (D-I).** GO only if all four hold:

| Criterion | Where to read it |
|---|---|
| Availability ≥ 99% over the 24 h | `report`'s `## Verdict`, the D-A (Availability) line |
| 0 manual interventions | `report`'s `## Verdict`, the D-D (Manual interventions) line |
| ≥ 20 node spots in the window | `shadow-compare`'s `## Agreement`, the denominator of the Uncorroborated line |
| ≤ 10% of node spots uncorroborated | `shadow-compare`'s `## Agreement`, the **Uncorroborated** line's percentage (run with `--missed-spots`, so recorder gaps count against it) |

"Uncorroborated" means no RBN spotter (other than this node, which `shadow-compare.py` excludes
by every `deCall` it saw) reported that call within the tolerances. Look at the
`## Node-only, uncorroborated` list too: repeated fixed-frequency busts point at a decoder or RF
problem, not at stations only this node heard.

Frequency calibration: in `shadow-compare`'s `## Calibration` table, the `Freq delta (Hz)` row's
`Median` column is the signed median Δf, node minus primary. If its magnitude is over 20 Hz, set

```
freq_correction_ppm = -(median Δf) / freq_hz × 1e6
```

(for example +20 Hz at 7030000 Hz gives about -2.8), then make a planned restart:

```sh
sudo -u manta python3 /opt/manta/scripts/field-node.py note \
  --ledger /var/lib/manta-field/ledger.jsonl --kind planned --reason "stage 1 freq calibration"
sudoedit /etc/manta/manta-field.toml
manta config check --config /etc/manta/manta-field.toml
sudo systemctl restart manta-field.service
```

On **NO-GO**: fix the cause and repeat Stage 1 from a new `start` note. Do not join the
Aggregator.

If the node's band is 40 m, keep this 24 h ledger and report: they are also evidence for
ROADMAP M2's 24 h live-SDR soak gate.

## 6. Stage 2: join the Aggregator

1. The Aggregator operator adds the node on Aggregator's *Secondary Skimmers* tab (manual §9.1:
   up to eight secondaries, numbered 1 to 8; secondaries work only while the primary is
   connected), with the node's IP, port 7300 and a login callsign.
2. In Aggregator's *Skimmer Traffic* tab (§6.1), the node's spots appear prefixed with its
   secondary index (1 to 8) and marked `+` when forwarded.
3. On the node, manta logs the handshake and the ledger sees the client:

   ```sh
   journalctl -u manta-field -g 'command received' --since "10 min ago"  # expect command=Sett
   tail -n 1 /var/lib/manta-field/ledger.jsonl  # expect telnet_clients >= 1
   ```

   With no `SETT` within about 5 minutes, Aggregator does not forward this node's spots
   (manual §9.2); see section 11.
4. Start the 30-day clock:

   ```sh
   sudo -u manta python3 /opt/manta/scripts/field-node.py note \
     --ledger /var/lib/manta-field/ledger.jsonl --kind start --reason "aggregator secondary #1"
   ```

**The 30-day clock starts at this note.** Record its timestamp; every later window starts there.

## 7. Running the 30 days

Definitions (D-D, D-E, D-G):

- **Planned restart.** Write the note *before* restarting. Its window runs from the note until
  the first healthy sample after it, capped at 1800 s, and is excluded from availability.

  ```sh
  sudo -u manta python3 /opt/manta/scripts/field-node.py note \
    --ledger /var/lib/manta-field/ledger.jsonl --kind planned --reason "kernel update reboot"
  ```

- **Manual intervention.** Any other human action on the node: a USB replug, a hand-run restart
  without a planned note, a config edit, an unplanned reboot. Log it, then fix the cause. It
  fails scenario 1, so restart the clock with a new `start` note.

  ```sh
  sudo -u manta python3 /opt/manta/scripts/field-node.py note \
    --ledger /var/lib/manta-field/ledger.jsonl --kind manual --reason "USB replug, SDR not enumerated"
  ```

- **Watchdog recovery.** `manta-field-ledger.service` runs `manta-field-recover` once the node
  has been continuously bad (metrics unreachable, or any `manta_source_health == 0`) for 600 s,
  then waits at least 1800 s, at most 6 times per UTC day. Recoveries are logged in the ledger
  and listed by `report`. They are not manual interventions, but their downtime counts against
  availability.

Check progress weekly, with `--min-days` set to the days elapsed (7, 14, 21):

```sh
python3 /opt/manta/scripts/field-node.py report \
  --ledger /var/lib/manta-field/ledger.jsonl --spots-dir /var/lib/manta-field/spots --min-days 7
```

What to do with what it shows:

- **Unexplained restarts** (a new process start with no planned note and no watchdog recovery
  before it). Classify each from the journal and keep the classification for the field report:

  ```sh
  journalctl -u manta-field --since "2026-11-08 00:00" | grep -E "Main process exited|status="
  ```

  `status=101` is a decode-pipeline panic (file an issue with the journal lines). `status=1` is a
  start-up source-open failure (manta exits at once if the SDR cannot be opened at start; see
  section 11). A listener-task panic is logged but does not exit the process.
- **Source outages.** `## Source outages` counts them from `manta_source_outages_total` and
  `manta_source_down_seconds_total`, so drops shorter than the 60 s sample interval still show.
- **Recorder coverage below 99%** (spots recorded vs spots emitted). The JSON port has no history
  for late subscribers, so spots emitted while the recorder is disconnected are lost to it. Check
  `systemctl status manta-field-spots` and `journalctl -u manta-field-spots`, and the
  `manta_spots_dropped_lagged_total` counter.

## 8. Day 7: record the comparison (scenario 2)

Download the daily archives for every UTC date from the start note to 7 days after it:

```sh
cd ~/rbn
for i in 0 1 2 3 4 5 6 7; do
  d=$(date -u -d "2026-11-03 +$i day" +%Y%m%d)   # the start note's UTC date
  curl -fsSO "https://data.reversebeacon.net/rbn_history/$d.zip"
done
```

Run the comparison over the first 7 days:

```sh
PRIMARY=K1ABC                # the primary's RBN spotter callsign
START=2026-11-03T09:15:00Z   # the stage 2 start note's time
END=2026-11-10T09:15:00Z     # START + 7 days
python3 /opt/manta/scripts/shadow-compare.py \
  --node-spots /var/lib/manta-field/spots \
  --rbn ~/rbn/*.zip \
  --primary "$PRIMARY" --passband-khz 7000-7060 \
  --start "$START" --end "$END" \
  --json ~/day7-shadow.json > ~/day7-shadow.md
```

Open a docs PR adding `docs/DECISIONS/<date>-man96-30-day-field-run.md` from the template in
section 10, with the "Timeline" and "Scenario 2 (day 7)" sections filled in and the
`shadow-compare` markdown pasted verbatim. This PR records scenario 2; the run continues.

## 9. Day 30: record the run (scenario 1)

```sh
sudo -u manta python3 /opt/manta/scripts/field-node.py note \
  --ledger /var/lib/manta-field/ledger.jsonl --kind end --reason "day 30"
python3 /opt/manta/scripts/field-node.py report \
  --ledger /var/lib/manta-field/ledger.jsonl --spots-dir /var/lib/manta-field/spots \
  --min-days 30 > ~/day30-report.md; echo "exit $?"
python3 /opt/manta/scripts/field-node.py report \
  --ledger /var/lib/manta-field/ledger.jsonl --spots-dir /var/lib/manta-field/spots \
  --min-days 30 --format json > ~/day30-report.json
```

Exit 0 is PASS. Then download the daily archives for the whole run and compare the full window:

```sh
cd ~/rbn
for i in $(seq 0 30); do
  d=$(date -u -d "2026-11-03 +$i day" +%Y%m%d)   # the stage 2 start note's UTC date
  curl -fsSO "https://data.reversebeacon.net/rbn_history/$d.zip"
done
PRIMARY=K1ABC                # the primary's RBN spotter callsign
START=2026-11-03T09:15:00Z   # the stage 2 start note's time
END=2026-12-03T09:15:00Z     # the end note's time
python3 /opt/manta/scripts/shadow-compare.py \
  --node-spots /var/lib/manta-field/spots \
  --rbn ~/rbn/*.zip \
  --primary "$PRIMARY" --passband-khz 7000-7060 \
  --start "$START" --end "$END" \
  --json ~/day30-shadow.json > ~/day30-shadow.md
```

Hash the raw evidence (D-N). It stays on the node and in your archive, not in git:

```sh
cd /var/lib/manta-field
ls -l ledger.jsonl spots/
sha256sum ledger.jsonl spots/spots-*.jsonl ~/rbn/*.zip
```

Fill in the field report's remaining sections and open the second docs PR. In the same PR:

- Update ROADMAP gate lines **only where the evidence meets them**: the M2 24 h soak gate needs
  a 40 m run; the M3 7-day soak gate needs 7 days of "feeding spots continuously" (the Aggregator
  connected and ≥ 1 spot every UTC day).
- Update the `CLAUDE.md` Status paragraph to say what the run showed, and no more.

On FAIL, the report records why. Fix the cause and restart from Stage 2 with a new `start` note.

## 10. Field report template

`docs/DECISIONS/<date>-man96-30-day-field-run.md` must have these sections:

- **Node.** SDR hardware, antenna and splitter, host (CPU, OS, kernel), manta git SHA and build
  features, the config with the callsign and operator fields sanitized, the primary's call and
  SkimSrv/CW Skimmer version, the Aggregator version and the node's secondary index.
- **Timeline.** Stage 0, Stage 1 and Stage 2 dates; the go/no-go numbers (availability, manual
  interventions, node spots, uncorroborated share); any frequency correction applied.
- **Scenario 1.** The day-30 `report` output, verbatim, and the classification of every
  unexplained restart.
- **Scenario 2.** The day-7 and day-30 `shadow-compare` output, verbatim.
- **Interpretation and limitations.** One rig and one antenna; the known fading-robustness gap
  (MAN-107 through MAN-113); the decoder version and `decode.engine` in use.
- **Raw evidence.** For each file: name, bytes, SHA-256 and where it is archived.
- **Follow-ups.** Tickets for every defect the run found.

## 11. Troubleshooting

- **SDRplay service wedge.** The SDR driver service stops delivering samples and reopening does
  not clear it ([the 90 min soak record](../DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md)).
  On the node it shows as `manta_source_health == 0` that does not clear. The watchdog restarts
  the driver service and manta after 600 s; see its ledger records with
  `grep '"kind": *"recovery"' /var/lib/manta-field/ledger.jsonl` and
  `journalctl -u manta-field-ledger`. If recoveries hit the 6-per-day cap, the device probably
  needs a USB replug, which is a manual intervention. On a macOS bench, the equivalent of the
  recovery script is `sudo launchctl kickstart -k system/com.sdrplay.service`.
- **Source open failure at boot.** manta exits with status 1 if the SDR cannot be opened at
  start-up; systemd restarts it every 10 s (`StartLimitIntervalSec=0` means it never gives up).
  While it loops the metrics port is unreachable, and the watchdog treats that as bad and runs
  the recovery after 600 s.
- **The Aggregator drops manta.** The ledger's `telnet_clients` goes to 0 and `report`'s
  Aggregator-connected share falls. Check the firewall still admits the Aggregator host and that
  the journal shows `command=Sett` after each reconnect: with no `SETT` within about 5 minutes,
  Aggregator does not forward (manual §9.2,
  [the MAN-86 record](../DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md)).
- **`/healthz` says `unhealthy`.** The per-check lines say which source, listener or decode check
  failed; [node-health.md](node-health.md) explains each.
