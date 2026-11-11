#!/usr/bin/env python3
"""Generate grafana/dashboards/lgh-gateway-health.json (Grafana 10/11 schema). Run from the repo root."""
import json
from pathlib import Path

DS = {"type": "prometheus", "uid": "prometheus"}
panels = []
_id = [0]


def nid():
    _id[0] += 1
    return _id[0]


def target(expr, legend="", ref="A"):
    return {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref, "editorMode": "code", "range": True}


def stat(title, expr, x, y, w=4, h=4, unit="none", thresholds=None, mappings=None, decimals=None, desc=""):
    steps = thresholds or [{"color": "green", "value": None}]
    fc = {"unit": unit, "thresholds": {"mode": "absolute", "steps": steps}, "mappings": mappings or []}
    if decimals is not None:
        fc["decimals"] = decimals
    return {
        "id": nid(), "type": "stat", "title": title, "description": desc, "datasource": DS,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "fieldConfig": {"defaults": fc, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                    "colorMode": "background", "graphMode": "none", "textMode": "auto"},
        "targets": [target(expr)],
    }


def ts(title, targets, x, y, w=12, h=8, unit="none", desc="", stack=False, minv=None, maxv=None, draw="line"):
    custom = {"lineWidth": 2, "fillOpacity": 0, "showPoints": "never", "spanNulls": True}
    if stack:
        custom["stacking"] = {"mode": "normal", "group": "A"}
        custom["fillOpacity"] = 55
    fc = {"unit": unit, "custom": custom}
    if minv is not None:
        fc["min"] = minv
    if maxv is not None:
        fc["max"] = maxv
    return {
        "id": nid(), "type": "timeseries", "title": title, "description": desc, "datasource": DS,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "fieldConfig": {"defaults": fc, "overrides": []},
        "options": {"legend": {"displayMode": "list", "placement": "bottom"}, "tooltip": {"mode": "multi"}},
        "targets": [target(e, l, chr(65 + i)) for i, (e, l) in enumerate(targets)],
    }


def row(title, y):
    return {"id": nid(), "type": "row", "title": title, "gridPos": {"x": 0, "y": y, "w": 24, "h": 1}, "collapsed": False, "panels": []}


UPDOWN = [{"type": "value", "options": {"0": {"text": "DOWN", "color": "red"}, "1": {"text": "UP", "color": "green"}}}]
RED_GREEN = [{"color": "red", "value": None}, {"color": "green", "value": 1}]

# ---- row 0: status
panels.append(row("Status", 0))
panels += [
    stat("Bridge (PULL_ACK)", "min(lgh_bridge_up)", 0, 1, mappings=UPDOWN, thresholds=RED_GREEN,
         desc="1 while the UDP bridge acknowledges PULL_DATA within --bridge-timeout."),
    stat("Forwarder", "min(lgh_forwarder_up)", 4, 1, mappings=UPDOWN, thresholds=RED_GREEN),
    stat("Uplinks / min", "sum(rate(lgh_rx_packets_total{crc='ok'}[2m])) * 60", 8, 1, unit="short", decimals=0),
    stat("CRC error ratio", "max(lgh_crc_error_ratio)", 12, 1, unit="percentunit", decimals=1,
         thresholds=[{"color": "green", "value": None}, {"color": "yellow", "value": 0.05}, {"color": "red", "value": 0.2}],
         desc="1 - rxok/rxnb over the last 10 forwarder stat reports. Needs the forwarder's own counters: bad-CRC packets are usually not forwarded."),
    stat("Worst TX duty vs limit", "max(lgh_tx_duty_cycle_ratio / lgh_tx_duty_cycle_limit_ratio)", 16, 1, unit="percentunit", decimals=1,
         thresholds=[{"color": "green", "value": None}, {"color": "yellow", "value": 0.6}, {"color": "red", "value": 0.9}],
         desc="Highest sub-band usage as a share of its EU868 limit (1 h window)."),
    stat("Uptime", "max(lgh_uptime_seconds)", 20, 1, unit="s"),
]

# ---- row 1: radio
panels.append(row("Radio", 5))
panels += [
    ts("Uplinks per minute by SF", [("sum by (sf) (rate(lgh_rx_packets_total{crc='ok'}[2m])) * 60", "{{sf}}")], 0, 6, unit="short", stack=True),
    ts("CRC error ratio (forwarder stat)", [("lgh_crc_error_ratio", "windowed"),
                                            ("1 - sum(increase(lgh_forwarder_rx_crc_ok_total[1m])) / sum(increase(lgh_forwarder_rx_received_total[1m]))", "1 min")],
       12, 6, unit="percentunit", minv=0, desc="Burst of failed CRCs: interference or a failing front end."),
    ts("RSSI median by SF", [("histogram_quantile(0.5, sum by (le, sf) (rate(lgh_rx_rssi_dbm_bucket[2m])))", "{{sf}}")], 0, 14, unit="dBm", minv=-130, maxv=-40,
       desc="Bucket-interpolated median per spreading factor; resolution is the bucket width."),
    ts("SNR median by SF", [("histogram_quantile(0.5, sum by (le, sf) (rate(lgh_rx_snr_db_bucket[2m])))", "{{sf}}")], 12, 14, unit="dB"),
    ts("Uplink airtime by channel (share of time)", [("sum by (channel_mhz) (rate(lgh_rx_airtime_seconds_total[2m]))", "{{channel_mhz}} MHz")],
       0, 22, unit="percentunit", minv=0, desc="Estimated time on air of received uplinks (Semtech formula) as a share of time, per channel."),
    ts("TX duty cycle, share of the sub-band limit (1 h window)",
       [("lgh_tx_duty_cycle_ratio{band!='other'} / on (band) lgh_tx_duty_cycle_limit_ratio", "{{band}}"), ("vector(1)", "limit")],
       12, 22, unit="percentunit", minv=0, desc="Downlink airtime estimated from PULL_RESP orders passing the tap, divided by the EU868 sub-band limit (g 1 %, g1 1 %, g2 0.1 %, g3 10 %, g4 1 %). 100 % = at the limit."),
]

# ---- row 2: backhaul
panels.append(row("Backhaul and downlink", 30))
panels += [
    ts("Bridge reachability", [("lgh_bridge_up", "bridge up"), ("lgh_forwarder_up", "forwarder up")], 0, 31, unit="none", minv=0, maxv=1.1,
       desc="Drops to 0 when the bridge stops answering PULL_DATA (the demo bridge goes silent for 60 s every 10 minutes)."),
    ts("PUSH_DATA ack ratio", [("sum(rate(lgh_datagrams_total{type='push_ack'}[2m])) / sum(rate(lgh_datagrams_total{type='push_data'}[2m]))", "acked share"),
                               ("lgh_forwarder_upstream_ack_ratio", "forwarder ackr")], 12, 31, unit="percentunit", minv=0, maxv=1.05),
    ts("Seconds since last bridge ack", [("time() - lgh_bridge_last_ack_timestamp_seconds", "{{type}}")], 0, 39, unit="s", minv=0),
    ts("Downlink orders and results", [("rate(lgh_tx_requests_total[2m]) * 60", "orders / min"),
                                       ("sum by (error) (rate(lgh_tx_ack_total[2m])) * 60", "TX_ACK {{error}} / min")], 12, 39, unit="short", minv=0),
]

# ---- row 3: host
panels.append(row("Host", 47))
panels += [
    ts("CPU busy", [("lgh_cpu_busy_ratio", "busy")], 0, 48, w=6, unit="percentunit", minv=0, maxv=1),
    ts("Memory available", [("lgh_memory_available_bytes", "available"), ("lgh_memory_total_bytes", "total")], 6, 48, w=6, unit="bytes", minv=0),
    ts("Disk free", [("lgh_disk_avail_bytes", "{{path}}")], 12, 48, w=6, unit="bytes", minv=0),
    ts("Temperature", [("lgh_temperature_celsius", "{{sensor}}")], 18, 48, w=6, unit="celsius",
       desc="Kernel thermal zones plus --temp-file sensors. The packet-forwarder protocol does not carry the concentrator temperature."),
    ts("Exporter resident memory", [("lgh_exporter_resident_memory_bytes", "RSS")], 0, 56, w=6, unit="bytes", minv=0),
    ts("Parse errors and relay errors", [("sum by (kind) (increase(lgh_parse_errors_total[5m]))", "parse {{kind}}"),
                                         ("sum by (kind) (increase(lgh_relay_errors_total[5m]))", "relay {{kind}}")], 6, 56, w=18, unit="short", minv=0),
]

dash = {
    "uid": "lgh-gateway-health",
    "title": "LoRaWAN gateway health",
    "tags": ["lorawan", "gateway", "lgh-exporter"],
    "timezone": "browser",
    "schemaVersion": 39,
    "version": 1,
    "refresh": "5s",
    "time": {"from": "now-30m", "to": "now"},
    "panels": panels,
    "templating": {"list": []},
    "annotations": {"list": []},
}

out = Path(__file__).resolve().parent.parent / "grafana" / "dashboards" / "lgh-gateway-health.json"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(dash, indent=2) + "\n")
print(f"wrote {out} ({len(panels)} panels)")
