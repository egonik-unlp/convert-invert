#!/usr/bin/env bash
for num in {1..10}; do
  echo $num
  convert-invert $num
done
