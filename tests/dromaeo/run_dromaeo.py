#!/usr/bin/env python

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

from http.server import HTTPServer, SimpleHTTPRequestHandler
import os
import subprocess
import sys
import urllib
import json
import urllib.parse


# Port to run the HTTP server on for Dromaeo.
TEST_SERVER_PORT = 8192


# Run servo and print / parse the results for a specific Dromaeo module.
def run_servo(servo_exe, tests):
    url = "\"http://localhost:{0}/dromaeo/web/index.html?{1}&automated&post_json\"".format(TEST_SERVER_PORT, tests)
    subprocess.run(["hdc", "shell", "aa", "force-stop", "org.servo.servoshell"], check=True)

    res = subprocess.run(["hdc", "fport", "ls"], check=True, encoding='utf-8', capture_output=True)
    if f"tcp:{TEST_SERVER_PORT} tcp:{TEST_SERVER_PORT}" not in res.stdout:
        # Forward the test serverport from the phone to our host
        res = subprocess.run(["hdc", "rport", f"tcp:{TEST_SERVER_PORT}", f"tcp:{TEST_SERVER_PORT}"], check=True, encoding='utf-8', capture_output=True)

    if "[Fail]" in res.stdout:
        raise RuntimeError(f"Failed to forward port: stdout: {res.stdout}, stderr: {res.stderr}")

    args = ["hdc", "shell", "aa", "start", "-a", "EntryAbility", "-b" "org.servo.servoshell", "-U", url ] #"--psn=-z", "--psn=-f"]
    return subprocess.Popen(args)


# Print usage if command line args are incorrect
def print_usage():
    print("USAGE: {0} tests servo_binary dromaeo_base_dir [BMF JSON output]".format(sys.argv[0]))


post_data = None


# Handle the POST at the end
class RequestHandler(SimpleHTTPRequestHandler):
    def do_POST(self):
        global post_data
        self.send_response(200)
        self.log_message("Finished sending response!")
        self.end_headers()
        self.wfile.write(b"<HTML>POST OK.<BR><BR>")
        length = int(self.headers.get('content-length'))
        parameters = urllib.parse.parse_qs(self.rfile.read(length))
        post_data = parameters[b'data']

    def log_message(self, format, *args):
        print(format % args)
        return


if __name__ == '__main__':
    if len(sys.argv) == 4 or len(sys.argv) == 5:
        tests = sys.argv[1]
        servo_exe = sys.argv[2]
        base_dir = sys.argv[3]
        bmf_output = ""
        if len(sys.argv) == 5:
            bmf_output = sys.argv[4]
        os.chdir(base_dir)

        # Ensure servo binary can be found
        if not os.path.isfile(servo_exe):
            print("Unable to find {0}. This script expects an existing build of Servo.".format(servo_exe))
            sys.exit(1)

        # Start the test server
        server = HTTPServer(('', TEST_SERVER_PORT), RequestHandler)

        print("Testing Dromaeo on Servo!")
        proc = run_servo(servo_exe, tests)
        server.timeout = 10
        while not post_data:
            print("Before handle request")
            server.handle_request()
            print("After handle request")
        data = json.loads(post_data[0])
        number = 0
        length = 0
        for test in data:
            number = max(number, len(data[test]))
            length = max(length, len(test))
        print("\n Test{0} | Time".format(" " * (length - len("Test"))))
        print("-{0}-|-{1}-".format("-" * length, "-" * number))
        for test in data:
            print(" {0}{1} | {2}".format(test, " " * (length - len(test)), data[test]))
        if bmf_output:
            output = dict()
            for (k, v) in data.items():
                output[f"Dromaeo/{k}"] = {'throughput': {'value': float(v)}}
            with open(bmf_output, 'w', encoding='utf-8') as f:
                json.dump(output, f, indent=4)
        proc.kill()
    else:
        print_usage()
