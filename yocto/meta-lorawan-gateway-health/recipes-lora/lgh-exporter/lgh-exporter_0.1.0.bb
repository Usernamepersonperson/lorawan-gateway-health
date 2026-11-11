# UNVERIFIED: this recipe has never been built with BitBake. It follows the conventions of the
# usual Yocto Rust recipes: `inherit cargo cargo-update-recipe-crates`, RUST_PANIC_STRATEGY = "abort",
# binary under /opt, a systemd unit. Expect to fix small things on the first build.

SUMMARY = "Prometheus exporter for LoRaWAN gateway and packet-forwarder health"
DESCRIPTION = "UDP tap for the Semtech packet-forwarder protocol that exposes uplink/downlink counters, CRC errors, RSSI/SNR histograms, airtime and duty-cycle estimates, backhaul status and host metrics on a Prometheus endpoint."
HOMEPAGE = "https://github.com/Usernamepersonperson/lorawan-gateway-health"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://LICENSE;md5=69595eb90c9890631dd8fe22bfc7a018"

SRC_URI = "git://github.com/Usernamepersonperson/lorawan-gateway-health.git;protocol=https;branch=main"
# Pin to a commit before building for real; AUTOREV only keeps this example short.
SRCREV = "${AUTOREV}"
S = "${WORKDIR}/git"

SRC_URI += " \
    file://lgh-exporter.service \
"

require lgh-exporter-crates.inc

inherit cargo cargo-update-recipe-crates systemd

# Same setting common Rust recipes use (smaller binary, no unwinding tables).
RUST_PANIC_STRATEGY = "abort"

# Only the exporter is shipped; lgh-replay is a development tool.
CARGO_BUILD_FLAGS:append = " --bin lgh-exporter"

# Defaults; override in your distro configuration.
optdir ?= "/opt"
sysconfoptdir ?= "/etc/opt"

do_install() {
    install -m 0755 -d ${D}${optdir}/lgh-exporter
    install -m 0755 ${B}/target/${CARGO_TARGET_SUBDIR}/lgh-exporter \
        ${D}${optdir}/lgh-exporter/lgh-exporter

    # systemd unit; enable with `systemctl enable lgh-exporter` or via SYSTEMD_AUTO_ENABLE.
    install -m 0755 -d ${D}${systemd_system_unitdir}
    install -m 0644 ${WORKDIR}/lgh-exporter.service ${D}${systemd_system_unitdir}/lgh-exporter.service
}

FILES:${PN} += "${optdir}/lgh-exporter"
SYSTEMD_SERVICE:${PN} = "lgh-exporter.service"
