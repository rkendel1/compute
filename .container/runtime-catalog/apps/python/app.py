import json
import platform

print(json.dumps({"runtime": "python", "version": platform.python_version(), "portable": True}))
