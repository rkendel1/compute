require 'json'
op = ARGV.shift
if op == 'stdin'
  STDOUT.write(STDIN.read)
elsif op == 'exit'
  exit 7
elsif op == 'sleep'
  sleep 5
elsif op == 'certify'
  result = {
    input: File.read(File.join(ENV.fetch('COMPUTE_WORK_DIR'), 'hello.txt')),
    success: true,
    argument: ARGV[0],
    environment: ENV.fetch('CERTIFICATION_ENV', 'missing'),
    host_environment: ENV.fetch('COMPUTE_HOST_SECRET', 'missing')
  }
  File.write(File.join(ENV.fetch('COMPUTE_OUTPUT_DIR'), 'result.json'), JSON.generate(result))
  puts JSON.generate(runtime: 'ruby', runtime_version: RUBY_VERSION)
  warn 'certification-stderr'
end
