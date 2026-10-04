"""Bind every persona feature: a scenario added to features/ runs automatically,
and fails until each of its steps has a definition in steps/."""

from pytest_bdd import scenarios

scenarios("features")
