"""Synthetic APK archive inputs for offline harness tests."""

import io
import tarfile


def archive(files):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
        end = tar.offset
    return buf.getvalue()[:end]
