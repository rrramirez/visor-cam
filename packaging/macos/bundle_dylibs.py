#!/usr/bin/env python3
"""Copy every non-system dylib a Mach-O binary depends on into a bundle's
Frameworks directory and point all load commands at the bundled copies.

    bundle_dylibs.py <executable> <Frameworks dir>

System libraries (/usr/lib, /System) and the Swift runtime stay external.
"""
import os
import shutil
import subprocess
import sys

SYSTEM_PREFIXES = ("/usr/lib/", "/System/")


def deps(path):
    out = subprocess.run(["otool", "-L", path], check=True, capture_output=True, text=True).stdout
    result = []
    for line in out.splitlines()[1:]:
        dep = line.strip().split(" (compatibility")[0]
        if dep and not dep.startswith(SYSTEM_PREFIXES):
            result.append(dep)
    return result


def rpaths(path):
    out = subprocess.run(["otool", "-l", path], check=True, capture_output=True, text=True).stdout.splitlines()
    found = []
    for i, line in enumerate(out):
        if line.strip() == "cmd LC_RPATH":
            found.append(out[i + 2].strip().split("path ")[1].split(" (offset")[0])
    return found


def resolve(dep, loader):
    if dep.startswith("@rpath/"):
        name = dep[len("@rpath/"):]
        for rp in rpaths(loader) + ["/opt/homebrew/lib", "/usr/local/lib"]:
            rp = rp.replace("@loader_path", os.path.dirname(loader)).replace("@executable_path", os.path.dirname(loader))
            candidate = os.path.join(rp, name)
            if os.path.exists(candidate):
                return os.path.realpath(candidate)
        return None
    if dep.startswith("@loader_path/"):
        return os.path.realpath(os.path.join(os.path.dirname(loader), dep[len("@loader_path/"):]))
    if dep.startswith("@"):
        return None
    return os.path.realpath(dep)


def main():
    exe, frameworks = sys.argv[1], sys.argv[2]
    os.makedirs(frameworks, exist_ok=True)
    queue = [exe]
    seen = set()
    while queue:
        binary = queue.pop()
        for dep in deps(binary):
            src = resolve(dep, binary)
            if src is None:
                if dep.startswith("@rpath/libswift"):
                    continue
                print(f"warning: cannot resolve {dep} (from {binary})", file=sys.stderr)
                continue
            name = os.path.basename(dep)
            dst = os.path.join(frameworks, name)
            if name not in seen:
                seen.add(name)
                shutil.copy2(src, dst)
                os.chmod(dst, 0o755)
                subprocess.run(["install_name_tool", "-id", f"@rpath/{name}", dst], check=True, capture_output=True)
                queue.append(dst)
            subprocess.run(["install_name_tool", "-change", dep, f"@rpath/{name}", binary], check=True, capture_output=True)
    subprocess.run(["install_name_tool", "-add_rpath", "@executable_path/../Frameworks", exe], capture_output=True)
    for name in seen:
        lib = os.path.join(frameworks, name)
        subprocess.run(["install_name_tool", "-add_rpath", "@loader_path", lib], capture_output=True)
    print(f"bundled {len(seen)} libraries")


if __name__ == "__main__":
    main()
