
import pytest

@pytest.fixture(scope="session")
def sess_counter(request):
    request.config._inits = getattr(request.config, "_inits", 0) + 1
    return request.config._inits
