"""Python 3 / build-rd7.py / rebuild RD7 TV 1.6.2 and the Studio resource."""
import pathlib
import shutil
import sys

root = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(root / 'webos'))
import build

config = dict(build.webapp.GENERIC, theme='dark', language='pt', currency='BRL',
              temperature='C', tmdbRegion='BR', plainBackground=True,
              showCrypto=False, fullMotion=True)
build.build(True, config)
resource = root.parent / 'src-tauri/resources/rd7-tv.ipk'
if resource.parent.exists():
    shutil.copy2(root / 'FreeMyTV-webos.ipk', resource)
    print('Updated Studio embedded IPK.')
