# Tails events.log, stamps each new line with CLOCK_MONOTONIC us on arrival (inotify-free poll at 1 ms).
import sys, time, os
p, out = sys.argv[1], open(sys.argv[2], 'a', buffering=1)
f = open(p, 'a+'); f.seek(0, 2)
while True:
    line = f.readline()
    if not line: time.sleep(0.001); continue
    out.write(f"{time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000} {line}")
