"""Tests for the channel interface."""
import pytest
from solomon.channels import Channel


def test_channel_is_abstract():
    """Channel should be abstract and not instantiable."""
    with pytest.raises(TypeError):
        Channel()


def test_channel_interface_defines_methods():
    """Channel should define discover and act as abstract methods."""
    assert hasattr(Channel, "discover")
    assert hasattr(Channel, "act")
    assert Channel.__abstractmethods__ == frozenset({"discover", "act"})
