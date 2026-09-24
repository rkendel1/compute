import java.io.*;
import java.nio.file.*;
public class Certification {
  static String env(String name) { String value = System.getenv(name); return value == null ? "missing" : value; }
  public static void main(String[] args) throws Exception {
    String op = args[0];
    if (op.equals("stdin")) System.in.transferTo(System.out);
    else if (op.equals("exit")) System.exit(7);
    else if (op.equals("sleep")) Thread.sleep(5000);
    else if (op.equals("certify")) {
      String input = Files.readString(Path.of(env("COMPUTE_WORK_DIR"), "hello.txt"));
      String result = String.format("{\"input\":\"%s\",\"success\":true,\"argument\":\"%s\",\"environment\":\"%s\",\"host_environment\":\"%s\"}", input, args[1], env("CERTIFICATION_ENV"), env("COMPUTE_HOST_SECRET"));
      Files.writeString(Path.of(env("COMPUTE_OUTPUT_DIR"), "result.json"), result);
      System.out.printf("{\"runtime\":\"jvm\",\"runtime_version\":\"%s\"}%n", System.getProperty("java.version"));
      System.err.println("certification-stderr");
    }
  }
}
