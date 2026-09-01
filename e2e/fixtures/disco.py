
import pytest


def test_one():
    assert True


def test_two():
    assert True


@pytest.mark.serial
def test_ser():
    assert True


@pytest.mark.parametrize("x", [1, 2])
def test_p(x):
    assert x
