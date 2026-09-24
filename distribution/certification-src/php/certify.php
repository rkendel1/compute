<?php
$op = $argv[1];
if ($op === 'stdin') echo stream_get_contents(STDIN);
elseif ($op === 'exit') exit(7);
elseif ($op === 'sleep') sleep(5);
elseif ($op === 'certify') {
  $result = [
    'input' => file_get_contents(getenv('COMPUTE_WORK_DIR') . '/hello.txt'),
    'success' => true,
    'argument' => $argv[2],
    'environment' => getenv('CERTIFICATION_ENV') === false ? 'missing' : getenv('CERTIFICATION_ENV'),
    'host_environment' => getenv('COMPUTE_HOST_SECRET') === false ? 'missing' : getenv('COMPUTE_HOST_SECRET')
  ];
  file_put_contents(getenv('COMPUTE_OUTPUT_DIR') . '/result.json', json_encode($result));
  echo json_encode(['runtime' => 'php', 'runtime_version' => PHP_VERSION]) . "\n";
  fwrite(STDERR, "certification-stderr\n");
}
?>
