#!/bin/bash
set -ex

echo "starting zookeeper"
/usr/local/kafka/kafka_2.12-3.5.0/bin/zookeeper-server-start.sh /usr/local/kafka/kafka_2.12-3.5.0/config/zookeeper.properties > /dev/null 2>&1 &
sleep 5
lsof -i:2181  > /dev/null || {
  echo "failed to start zookeeper"
  exit 1
}

echo "zookeeper started. starting kafka"
/usr/local/kafka/kafka_2.12-3.5.0/bin/kafka-server-start.sh /usr/local/kafka/kafka_2.12-3.5.0/config/server.properties > /dev/null 2>&1 &
sleep 5
lsof -i:9092 > /dev/null || {
  echo "kafka start failed"
  exit 1
}

/usr/local/kafka/kafka_2.12-3.5.0/bin/kafka-topics.sh --create --topic taxi --partitions 1 --replication-factor 1 --bootstrap-server=127.0.0.1:9092 \
  --if-not-exists  > /dev/null 2>&1 || {
  echo "kafka topic creation failed"
  exit 1
}
sleep 3

# use it in mfa
# pip install risingwave
# python3 server/udf.py > /dev/null 2>&1 &
# lsof -i:8815  > /dev/null || {
#   echo "failed to start udf"
#   exit 1
# }
# sleep 5

psql -U root -h 127.0.0.1 -p 4566 -d dev -a -f taxi-start.sql || {
  echo "failed to initialize db for taxi"
  exit 1
}
sleep 2

# use it in mfa
# export GENERATOR_PATH=generator
# python3 generator --num-users=15 \
#   --dump-users="$GENERATOR_PATH/users.json"

pip3 install -r server/model/requirements.txt