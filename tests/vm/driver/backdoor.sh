#!/bin/bash
# The VM test driver's root shell: announces each boot on the archix.shell virtio-serial port, then runs each "TOKEN TIMEOUT BASE64-COMMAND" line it reads there and answers "TOKEN STATUS BASE64-STDOUT BASE64-STDERR".
port=/dev/virtio-ports/archix.shell
until [ -e "$port" ]; do sleep 0.1; done
export HOME=/root
cd /
exec 3<> "$port"
echo "READY $(cat /proc/sys/kernel/random/boot_id)" >&3
while read -r token limit command <&3; do
  run=$(mktemp -d)
  base64 -d <<< "$command" > "$run/cmd"
  timeout --kill-after=10 "$limit" bash "$run/cmd" < /dev/null > "$run/out" 2> "$run/err"
  status=$?
  echo "$token $status $(base64 -w0 < "$run/out") $(base64 -w0 < "$run/err")" >&3
  rm -rf "$run"
done
