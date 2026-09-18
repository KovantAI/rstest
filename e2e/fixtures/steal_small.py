
# Tiny second file: exists only so lazy runs 2 workers (n is capped to the file
# count). The worker that collects and drains this file becomes the thief. It
# does not log, so the worker-spread assertion counts only the big file's items.
def test_tiny():
    assert True
