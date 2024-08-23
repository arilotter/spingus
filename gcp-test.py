import subprocess
import time
import signal
import sys
import argparse
from tqdm import tqdm

# List to store container IDs
containers = []


def cleanup():
    print("\nCleaning up containers...")
    for container in containers:
        subprocess.run(
            [
                "gcloud",
                "compute",
                "instances",
                "delete",
                container,
                "--project",
                args.project_id,
                "--quiet",
            ]
        )
    print("All containers terminated.")


def signal_handler(sig, frame):
    print("\nScript interrupted. Cleaning up...")
    cleanup()
    sys.exit(0)


# Register the signal handler
signal.signal(signal.SIGINT, signal_handler)
signal.signal(signal.SIGTERM, signal_handler)

# Parse command-line arguments
parser = argparse.ArgumentParser(description="Spin up Docker containers on GCP")
parser.add_argument("project_id", help="GCP project ID")
parser.add_argument("num_containers", type=int, help="Number of containers to spin up")
parser.add_argument("container_image", help="Docker image to use")
parser.add_argument(
    "--runtime", type=int, default=60, help="Runtime in seconds (default: 60)"
)
args = parser.parse_args()

try:
    # Spin up N containers
    for i in range(args.num_containers):
        container_name = f"container-{i}"
        print(f"Starting container {container_name}...")

        result = subprocess.run(
            [
                "gcloud",
                "compute",
                "instances",
                "create-with-container",
                container_name,
                "--project",
                args.project_id,
                "--container-image",
                args.container_image,
            ],
            capture_output=True,
            text=True,
        )

        if result.returncode == 0:
            containers.append(container_name)
            print(f"Container {container_name} started successfully.")
        else:
            print(f"Failed to start container {container_name}. Error: {result.stderr}")

    # Wait for specified time with progress bar
    print(f"Waiting for {args.runtime} seconds...")
    for _ in tqdm(range(args.runtime)):
        time.sleep(1)

finally:
    # Cleanup
    cleanup()

print("Script completed successfully.")
