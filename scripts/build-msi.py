#!/usr/bin/env python3
"""Build a genuine Windows .msi for enlil-bridge-agent with msibuild.

Assembles the MSI database from IDT table dumps (no WiX/wine needed):
writes one .idt per table, creates the embedded Data1.cab with gcab, then
drives `msibuild` to import the tables, attach the CAB stream and set the
summary information. Verifies the result with msiinfo/msidump.

Usage:
    build-msi.py <version> <exe> <config-toml> <outdir>
"""

import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import uuid

UPGRADE_CODE = "A2CCE516-61CB-4191-ADEA-80E7F9A9DD71"
EXE_COMPONENT_GUID = "9AA416E9-9098-478A-967C-27ABC381E47C"
CONFIG_COMPONENT_GUID = "52DE72E0-D723-45A8-9DF3-A46F5A37FEB0"
PRODUCT_NAME = "Enlil Bridge Agent"
MANUFACTURER = "Enlil"


def product_code(version):
    # Deterministic per version: reinstall/repair works for the same
    # version; bump the version and the code rotates.
    return str(uuid.uuid5(uuid.NAMESPACE_URL,
                          f"https://github.com/basic-automation/enlil/bridge-agent#{version}")) \
        .upper()


def idt(table, columns, keys, rows):
    """Render an IDT file in msitools' format.

    columns: list of (name, type); keys: primary-key column names;
    rows: list of lists (None -> NULL/empty).
    """
    lines = []
    lines.append("\t".join(c[0] for c in columns))
    lines.append("\t".join(c[1] for c in columns))
    lines.append("\t".join([table] + keys))
    for row in rows:
        lines.append("\t".join("" if v is None else str(v) for v in row))
    return "\n".join(lines) + "\n"


def file_hash_parts(path):
    digest = hashlib.md5(open(path, "rb").read()).digest()
    return list(struct.unpack("<4I", digest))


def build_tables(version, exe_path, config_path):
    exe_size = os.path.getsize(exe_path)
    config_size = os.path.getsize(config_path)
    exe_hash = file_hash_parts(exe_path)
    config_hash = file_hash_parts(config_path)

    tables = {}

    tables["Property"] = idt("Property", [
        ("Property", "s72"), ("Value", "s0"),
    ], ["Property"], [
        ["ProductCode", "{" + product_code(version) + "}"],
        ["ProductName", PRODUCT_NAME],
        ["ProductVersion", version],
        ["Manufacturer", MANUFACTURER],
        ["ProductLanguage", "1033"],
        ["ALLUSERS", "1"],
    ])

    tables["Directory"] = idt("Directory", [
        ("Directory", "s72"), ("Directory_Parent", "S72"),
        ("DefaultDir", "l72"),
    ], ["Directory"], [
        ["TARGETDIR", None, "SourceDir"],
        ["ProgramFiles64Folder", "TARGETDIR", "."],
        ["INSTALLDIR", "ProgramFiles64Folder", "Enlil"],
        ["CommonAppDataFolder", "TARGETDIR", "."],
        ["ENLILDATA", "CommonAppDataFolder", "Enlil"],
    ])

    tables["Component"] = idt("Component", [
        ("Component", "s72"), ("ComponentId", "S38"), ("Directory_", "s72"),
        ("Attributes", "i2"), ("Condition", "S255"), ("KeyPath", "S72"),
    ], ["Component"], [
        ["AgentExe", "{" + EXE_COMPONENT_GUID + "}", "INSTALLDIR",
         "256", None, "AgentExeFile"],
        ["AgentConfig", "{" + CONFIG_COMPONENT_GUID + "}", "ENLILDATA",
         "256", None, "AgentConfigFile"],
    ])

    tables["Feature"] = idt("Feature", [
        ("Feature", "s72"), ("Feature_Parent", "S72"), ("Title", "L64"),
        ("Description", "L255"), ("Display", "I2"), ("Level", "i2"),
        ("Directory_", "S72"), ("Attributes", "i2"),
    ], ["Feature"], [
        ["ProductFeature", None, PRODUCT_NAME,
         "In-guest agent for the Enlil inter-guest bridge.", "1", "1",
         "INSTALLDIR", "1"],
    ])

    tables["FeatureComponents"] = idt("FeatureComponents", [
        ("Feature_", "s72"), ("Component_", "s72"),
    ], ["Feature_", "Component_"], [
        ["ProductFeature", "AgentExe"],
        ["ProductFeature", "AgentConfig"],
    ])

    tables["File"] = idt("File", [
        ("File", "s72"), ("Component_", "s72"), ("FileName", "l255"),
        ("FileSize", "i4"), ("Version", "S72"), ("Language", "S20"),
        ("Attributes", "I2"), ("Sequence", "i2"),
    ], ["File"], [
        ["AgentExeFile", "AgentExe", "enlil~1.exe|enlil-bridge-agent.exe",
         str(exe_size), None, None, "512", "1"],
        ["AgentConfigFile", "AgentConfig", "bridge~1.toml|bridge-agent.toml",
         str(config_size), None, None, "0", "2"],
    ])

    tables["Media"] = idt("Media", [
        ("DiskId", "i2"), ("LastSequence", "i4"), ("DiskPrompt", "L64"),
        ("Cabinet", "S255"), ("VolumeLabel", "L32"), ("Source", "S72"),
    ], ["DiskId"], [
        ["1", "2", None, "#Data1.cab", None, None],
    ])

    tables["MsiFileHash"] = idt("MsiFileHash", [
        ("File_", "s72"), ("Options", "i2"), ("HashPart1", "i4"),
        ("HashPart2", "i4"), ("HashPart3", "i4"), ("HashPart4", "i4"),
    ], ["File_"], [
        ["AgentExeFile", "0"] + [str(p) for p in exe_hash],
        ["AgentConfigFile", "0"] + [str(p) for p in config_hash],
    ])

    tables["ServiceInstall"] = idt("ServiceInstall", [
        ("ServiceInstall", "s72"), ("Name", "s255"), ("DisplayName", "L255"),
        ("ServiceType", "i4"), ("StartType", "i4"), ("ErrorControl", "i4"),
        ("LoadOrderGroup", "S255"), ("Dependencies", "S255"),
        ("StartName", "S255"), ("Password", "S255"), ("Arguments", "S255"),
        ("Component_", "s72"), ("Description", "L255"),
    ], ["ServiceInstall"], [
        ["AgentService", "EnlilBridgeAgent", PRODUCT_NAME,
         "16", "2", "1", None, None, None, None, None, "AgentExe",
         "In-guest agent for the Enlil inter-guest bridge."],
    ])

    tables["ServiceControl"] = idt("ServiceControl", [
        ("ServiceControl", "s72"), ("Name", "s255"), ("Event", "i4"),
        ("Component_", "s72"), ("Wait", "I2"),
    ], ["ServiceControl"], [
        ["AgentServiceControl", "EnlilBridgeAgent", "161", "AgentExe", "1"],
    ])

    seq_cols = [("Action", "s72"), ("Condition", "S255"), ("Sequence", "I2")]

    def seq_rows(actions):
        return [[a, None, str(s)] for a, s in actions]

    tables["InstallExecuteSequence"] = idt(
        "InstallExecuteSequence", seq_cols, ["Action"], seq_rows([
            ("CostInitialize", 800), ("FileCost", 900),
            ("CostFinalize", 1000), ("InstallValidate", 1400),
            ("InstallInitialize", 1500), ("ProcessComponents", 1600),
            ("UnpublishFeatures", 1700), ("StopServices", 1900),
            ("DeleteServices", 2000), ("RemoveFiles", 2600),
            ("InstallFiles", 4000), ("StartServices", 5900),
            ("PublishFeatures", 6300), ("PublishProduct", 6400),
            ("InstallFinalize", 6600),
        ]))

    tables["InstallUISequence"] = idt(
        "InstallUISequence", seq_cols, ["Action"], seq_rows([
            ("CostInitialize", 800), ("FileCost", 900),
            ("CostFinalize", 1000), ("ExecuteAction", 1300),
        ]))

    tables["AdminExecuteSequence"] = idt(
        "AdminExecuteSequence", seq_cols, ["Action"], seq_rows([
            ("CostInitialize", 800), ("FileCost", 900),
            ("CostFinalize", 1000), ("InstallValidate", 1400),
            ("InstallInitialize", 1500), ("InstallFiles", 4000),
            ("InstallFinalize", 6600),
        ]))

    tables["AdminUISequence"] = idt(
        "AdminUISequence", seq_cols, ["Action"], seq_rows([
            ("CostInitialize", 800), ("FileCost", 900),
            ("CostFinalize", 1000), ("ExecuteAction", 1300),
        ]))

    tables["AdvtExecuteSequence"] = idt(
        "AdvtExecuteSequence", seq_cols, ["Action"], seq_rows([
            ("CostInitialize", 800), ("CostFinalize", 1000),
            ("InstallValidate", 1400), ("InstallInitialize", 1500),
            ("PublishFeatures", 6300), ("PublishProduct", 6400),
            ("InstallFinalize", 6600),
        ]))

    return tables


def run(cmd, **kw):
    print("+", " ".join(cmd))
    subprocess.run(cmd, check=True, **kw)


def main(argv):
    version, exe_path, config_path, outdir = argv[1:5]
    for tool in ("msibuild", "msiinfo", "msidump", "gcab"):
        if shutil.which(tool) is None:
            print(f"error: required tool '{tool}' not found", file=sys.stderr)
            return 1
    for path in (exe_path, config_path):
        if not os.path.isfile(path):
            print(f"error: input not found: {path}", file=sys.stderr)
            return 1

    workdir = tempfile.mkdtemp(prefix="enlil-msi-")
    try:
        tables = build_tables(version, exe_path, config_path)
        idt_paths = []
        for name, content in tables.items():
            path = os.path.join(workdir, f"{name}.idt")
            with open(path, "w") as f:
                f.write(content)
            idt_paths.append(path)

        # CAB: file order must match File.Sequence (exe=1, config=2).
        cab_path = os.path.join(workdir, "Data1.cab")
        run(["gcab", "--create", cab_path, exe_path, config_path])

        msi_name = f"enlil-bridge-agent-{version}-x64.msi"
        os.makedirs(outdir, exist_ok=True)
        msi_path = os.path.abspath(os.path.join(outdir, msi_name))
        if os.path.exists(msi_path):
            os.remove(msi_path)

        run(["msibuild", msi_path, "-s", PRODUCT_NAME, MANUFACTURER,
             "x64;1033", "{" + UPGRADE_CODE + "}"])
        for path in idt_paths:
            run(["msibuild", msi_path, "-i", path])
        run(["msibuild", msi_path, "-a", "Data1.cab", cab_path])

        # Verify: the database must round-trip through msidump/msiinfo.
        # (Run msidump with cwd=workdir so its .idt dump files don't
        # pollute the caller's directory.)
        info = subprocess.run(["msiinfo", "tables", msi_path],
                              check=True, capture_output=True, text=True)
        imported = set(info.stdout.split())
        missing = [t for t in tables if t not in imported]
        if missing:
            print(f"error: tables missing after build: {missing}",
                  file=sys.stderr)
            return 1
        dump = subprocess.run(["msidump", "--tables", msi_path],
                              check=True, capture_output=True, text=True,
                              cwd=workdir)
        for needle in ("EnlilBridgeAgent", "enlil-bridge-agent.exe",
                       "ProductVersion"):
            if needle not in dump.stdout:
                print(f"error: expected {needle!r} in dumped tables",
                      file=sys.stderr)
                return 1
        print(f"wrote {msi_path} "
              f"({os.path.getsize(msi_path)} bytes, "
              f"{len(imported)} tables verified)")
        return 0
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
