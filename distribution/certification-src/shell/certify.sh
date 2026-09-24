op=$1
if [ "$op" = stdin ]; then
  while IFS= read -r line || [ -n "$line" ]; do printf '%s' "$line"; done
elif [ "$op" = exit ]; then
  exit 7
elif [ "$op" = sleep ]; then
  read -r _ < /dev/null
  /proc/$$/exe sleep 5
elif [ "$op" = certify ]; then
  IFS= read -r input < "$COMPUTE_WORK_DIR/hello.txt" || true
  version=$(/proc/$$/exe 2>&1 || true)
  version=${version#*BusyBox v}
  version=${version%% *}
  printf '{"input":"%s","success":true,"argument":"%s","environment":"%s","host_environment":"%s"}' \
    "$input" "$2" "${CERTIFICATION_ENV-missing}" "${COMPUTE_HOST_SECRET-missing}" > "$COMPUTE_OUTPUT_DIR/result.json"
  printf '{"runtime":"shell","runtime_version":"busybox-%s"}\n' "$version"
  printf 'certification-stderr\n' >&2
fi
