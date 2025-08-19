#!/bin/bash

# Configuration
BS=256k
RUNTIME=30s
SIZES=("5M" "1G")
RW_TYPES=("randread" "read")
MAX_THREADS=256  # Maximum number of threads to test
BASE_THREADS=4   # Starting number of threads
ITERATIONS=10    # Number of times to repeat each test
# DIRS=("/tmp/mp-async" "/tmp/mp-sync")  # Test directories
DIRS=("/tmp/mp")  # Test directories

# Create temporary fio config file
create_fio_config() {
    local size=$1
    local rw_type=$2
    local threads=$3

    cat << EOF > temp_fio.conf
[global]
name=fs_bench_${size}_${rw_type}_${threads}threads
bs=$BS
runtime=$RUNTIME
ioengine=libaio
fallocate=none
time_based
group_reporting

[test]
size=$size
rw=$rw_type
numjobs=$threads
EOF
}

# Create results directory
RESULTS_DIR="fio_results_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"

# Run tests for each directory
for dir in "${DIRS[@]}"; do
    dir_type=$(basename "$dir" | cut -d'-' -f2)  # Extract 'async' or 'sync' from directory name
    echo "Testing directory: $dir (${dir_type})"

    for size in "${SIZES[@]}"; do
        for rw_type in "${RW_TYPES[@]}"; do
            threads=$BASE_THREADS
            while [ $threads -le $MAX_THREADS ]; do
                echo "Running test: $size $rw_type with $threads threads"

                # Create config file
                create_fio_config "$size" "$rw_type" "$threads"

                # Run the test ITERATIONS times
                for i in $(seq 1 $ITERATIONS); do
                    echo "  Iteration $i of $ITERATIONS"

                    # Run fio and save output
                    OUTPUT_FILE="$RESULTS_DIR/${dir_type}_${size}_${rw_type}_${threads}threads_iteration${i}.json"
                    fio --thread --directory="$dir" --output="$OUTPUT_FILE" --output-format=json temp_fio.conf

                    # Small delay between iterations
                    sleep 2
                done

                # Double the number of threads for next iteration
                threads=$((threads * 2))
            done
        done
    done
done

# Cleanup
rm temp_fio.conf

echo "Testing complete. Results are in $RESULTS_DIR"
