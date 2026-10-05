import serial
import json
import time

PORT = "COM3"
BAUDRATE = 115200


def send_command(ser, command):
    data = json.dumps(command, separators=(",", ":"))

    print(f"\n>>> {data}")

    ser.write((data + "\n").encode())
    ser.flush()

    response = ser.readline().decode(errors="replace").strip()

    print(f"<<< {response}")

    return response


def main():
    print(f"Opening {PORT} at {BAUDRATE} baud...")

    ser = serial.Serial(
        port=PORT,
        baudrate=BAUDRATE,
        timeout=2
    )

    try:
        time.sleep(1)
        ser.reset_input_buffer()

        # Test 1: Ping
        command = {
            "id": "1",
            "cmd": "ping"
        }

        send_command(ser, command)

        # Test 2: Capabilities
        command = {
            "id": "2",
            "cmd": "capabilities"
        }

        send_command(ser, command)

        # Test 3: GPIO13 HIGH
        command = {
            "id": "3",
            "cmd": "gpio_write",
            "args": {
                "pin": 13,
                "value": 1
            }
        }

        send_command(ser, command)

        # Test 3b: GPIO13 LOW
        command = {
            "id": "4",
            "cmd": "gpio_write",
            "args": {
                "pin": 13,
                "value": 0
            }
        }

        send_command(ser, command)

    finally:
        ser.close()


if __name__ == "__main__":
    main()