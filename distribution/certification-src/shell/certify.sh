op=$1
if [ "$op" = stdin ]; then
  while IFS= read -r line || [ -n "$line" ]; do printf '%s' "$line"; done
elif [ "$op" = exit ]; then
  exit 7
elif [ "$op" = sleep ]; then
  while :; do :; done
elif [ "$op" = certify ]; then
  IFS= read -r input < "$COMPUTE_WORK_DIR/hello.txt" || true
  printf '{"input":"%s","success":true,"argument":"%s","environment":"%s","host_environment":"%s"}' \
    "$input" "$2" "${CERTIFICATION_ENV-missing}" "${COMPUTE_HOST_SECRET-missing}" > "$COMPUTE_OUTPUT_DIR/result.json"
  printf '{"runtime":"shell","runtime_version":"%s"}\n' "$CERTIFICATION_VERSION"
  printf 'certification-stderr\n' >&2
fi
