
# Boot gw0 late, as a loaded 2-vCPU CI runner can: the thief must still find
# work left to steal (steal.py's items take long enough to outlast the delay).
import os
import time

if os.environ.get("RSTEST_WORKER_ID") == "gw0":
    time.sleep(0.5)
